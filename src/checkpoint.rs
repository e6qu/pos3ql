//! Save immutable table/index generations, publish their inventory, and recover it.
//!
//! Maintenance advances in bounded turns between client work and socket waits.
//! Captured output stays private until uploads and the conditional manifest write
//! finish. Readers and named backups retain objects that cleanup must preserve.

use crate::mem::mutex::Mutex;
use core::cmp::Ordering;
use std::task::Poll;

mod maintenance;
use maintenance::MetadataIo;
mod probe;
pub(crate) use probe::{Ticket as WriterProbeTicket, WriterProbe};

use crate::config::Config;
use crate::mem::arena::Arena;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::buffer::FixedBuf;
use crate::mem::fixed_map::FixedMap;
use crate::mem::fixed_vec::FixedVec;
use crate::object_store::{
    ByteRange, Client as ObjectStore, EntityTag, Error as ObjectError, ListCursor, Precondition,
};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::external::{ExternalRun, ExternalRunReader};
use crate::sql::types::ColType;
use crate::sql_err;
use crate::stack_format;
use crate::storage::{
    CheckpointValueCursor, ColumnDefault, ColumnMeta, MAX_COLUMNS, MAX_RELATION_COLUMNS,
    MaintenanceMode, MaintenanceStep, OwnedDatum, PartitionBound, PartitionBoundValue,
    PartitionDef, PartitionStrategy, RowCleanup, RowCleanupTarget, RowCleanupTask, RowHome,
    SerializedStoredQueryDependency, SqlName, Storage, TableDef, TransactionIdentityRecord,
};
use crate::store::{
    BlockId, BlockStore, BlockType, BlockWriter, MAX_ASSEMBLED, MAX_INLINE_ROW, OwnedObjectStore,
    RowSstFormat, SstCursor, SstHandle, SstKey, SstReader, SstVersionCursor, SstWriter, StackPlan,
    StoreError, TieredStore, ValueIndexHandle, ValueIndexStream, ValueIndexStreamStep,
    ValueIndexWriter, block_keys_at, copy_block_entry_at,
};
use crate::util::StackStr;
use crate::wal::crc32c::Crc32c;

pub(crate) const MANIFEST_KEY: &str = "manifest";
const COMMIT_HEAD_KEY: &str = "commit-head";
const WRITER_FENCE_KEY: &str = "writer-fence";
const RESTORE_PENDING_KEY: &str = "restore-pending";
const EXPORT_PENDING_KEY: &str = "export-pending";
const EXPORT_COMPLETE_KEY: &str = "export-complete";
const COMMIT_HEAD_HEADER: &str = "pos3ql-commit-head-v1";
const EMPTY_MANIFEST_HEADER: &str = "pos3ql-empty-manifest-v1";
const EMPTY_COMMIT_HEAD_HEADER: &str = "pos3ql-empty-commit-head-v1";
const EMPTY_MANIFEST: &[u8] = b"pos3ql-empty-manifest-v1\nend\n";
const EMPTY_COMMIT_HEAD: &[u8] = b"pos3ql-empty-commit-head-v1\nend\n";
const WRITER_FENCE_HEADER: &str = "pos3ql-writer-fence-v1";
const BACKUP_HEADER_V1: &str = "pos3ql-backup-v1";
const BACKUP_HEADER: &str = "pos3ql-backup-v2";
const EXPORT_HEADER: &str = "pos3ql-export-v1";
const POINT_IN_TIME_RESTORE_HEADER: &str = "pos3ql-point-in-time-restore-v1";
const BACKUP_NAME_BYTES: usize = 63;
const EXTENSION_PACKAGE_HEADER: &str = "pos3ql-extension-package-v1";
const VERSIONED_SST_ENTRY_HEADER: usize = 20; // rowid u64 | commit_lsn u64 | len u32
const VALUE_SORT_ENTRY_HEADER: usize = 8 + 8 + 8 + 4 + 4; // hash | rowid | lsn | key/payload lengths

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WriterToken(u128);

struct WriterFence {
    token: WriterToken,
    etag: EntityTag,
}

#[derive(Clone, Copy)]
enum PromotionRoots {
    Current,
    Validated {
        manifest: Option<EntityTag>,
        head: Option<EntityTag>,
    },
}

impl PromotionRoots {
    fn check(
        self,
        root: MutableRoot,
        current: Option<EntityTag>,
    ) -> Result<(), CheckpointSetupError> {
        if let Self::Validated { manifest, head } = self {
            let expected = match root {
                MutableRoot::Manifest => manifest,
                MutableRoot::CommitHead => head,
            };
            if current != expected {
                return Err(CheckpointSetupError::ObjectStore(format!(
                    "{} changed after startup validation",
                    root.key()
                )));
            }
        }
        Ok(())
    }
}

struct WriterPromotion {
    fence: WriterFence,
    manifest: EntityTag,
    head: EntityTag,
}

fn new_writer_token() -> std::io::Result<WriterToken> {
    use std::io::Read;

    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let token = u128::from_ne_bytes(bytes);
    if token == 0 {
        return Err(std::io::Error::other("random writer token was zero"));
    }
    Ok(WriterToken(token))
}

fn writer_fence_record(state: &str, token: WriterToken) -> StackStr<96> {
    stack_format!(
        96,
        "{WRITER_FENCE_HEADER}\nstate {state}\nwriter {:032x}\nend\n",
        token.0
    )
}

fn empty_manifest_record(token: WriterToken) -> StackStr<96> {
    stack_format!(96, "{EMPTY_MANIFEST_HEADER}\nw {:032x}\nend\n", token.0)
}

fn empty_commit_head_record(token: WriterToken) -> StackStr<128> {
    stack_format!(128, "{EMPTY_COMMIT_HEAD_HEADER}\nw {:032x}\nend\n", token.0)
}

fn parse_empty_root(
    bytes: &[u8],
    legacy: &[u8],
    header: &'static str,
) -> Result<Option<WriterToken>, &'static str> {
    if bytes == legacy {
        return Ok(None);
    }
    let text = core::str::from_utf8(bytes).map_err(|_| "empty root is not UTF-8")?;
    let mut lines = text.lines();
    if lines.next() != Some(header) {
        return Err("different root format");
    }
    let token = lines
        .next()
        .and_then(|line| line.strip_prefix("w "))
        .and_then(|word| u128::from_str_radix(word, 16).ok())
        .filter(|token| *token != 0)
        .map(WriterToken)
        .ok_or("bad empty-root writer")?;
    if lines.next() != Some("end") || lines.next().is_some() {
        return Err("bad empty-root terminator");
    }
    Ok(Some(token))
}

fn empty_manifest_writer(bytes: &[u8]) -> Result<Option<WriterToken>, &'static str> {
    parse_empty_root(bytes, EMPTY_MANIFEST, EMPTY_MANIFEST_HEADER)
}

fn empty_commit_head_writer(bytes: &[u8]) -> Result<Option<WriterToken>, &'static str> {
    parse_empty_root(bytes, EMPTY_COMMIT_HEAD, EMPTY_COMMIT_HEAD_HEADER)
}

fn is_empty_manifest(bytes: &[u8]) -> Result<bool, &'static str> {
    if bytes == EMPTY_MANIFEST {
        return Ok(true);
    }
    if bytes.starts_with(EMPTY_MANIFEST_HEADER.as_bytes()) {
        empty_manifest_writer(bytes)?;
        return Ok(true);
    }
    Ok(false)
}

fn is_empty_commit_head(bytes: &[u8]) -> Result<bool, &'static str> {
    if bytes == EMPTY_COMMIT_HEAD {
        return Ok(true);
    }
    if bytes.starts_with(EMPTY_COMMIT_HEAD_HEADER.as_bytes()) {
        empty_commit_head_writer(bytes)?;
        return Ok(true);
    }
    Ok(false)
}

fn parse_writer_fence(bytes: &[u8]) -> Result<(&str, WriterToken), &'static str> {
    let text = core::str::from_utf8(bytes).map_err(|_| "writer fence is not UTF-8")?;
    let mut lines = text.lines();
    if lines.next() != Some(WRITER_FENCE_HEADER) {
        return Err("bad writer fence header");
    }
    let state = lines
        .next()
        .and_then(|line| line.strip_prefix("state "))
        .filter(|state| matches!(*state, "active" | "transition"))
        .ok_or("bad writer fence state")?;
    let token = lines
        .next()
        .and_then(|line| line.strip_prefix("writer "))
        .and_then(|word| u128::from_str_radix(word, 16).ok())
        .filter(|token| *token != 0)
        .map(WriterToken)
        .ok_or("bad writer fence token")?;
    if lines.next() != Some("end") || lines.next().is_some() {
        return Err("bad writer fence terminator");
    }
    Ok((state, token))
}

pub(crate) const MANIFEST_HEADER: &str = "pos3ql-manifest-v34";

struct ManifestMetadata {
    lsn: u64,
    next_rowid: u64,
    latest_transaction_id: u64,
    catalog_sequence: u64,
    role_sequence: u64,
    next_large_object_oid: Option<crate::sql::ast::LargeObjectId>,
    writer: WriterToken,
    writer_range: (usize, usize),
}

fn parse_manifest_metadata(text: &str) -> Result<ManifestMetadata, &'static str> {
    let (header, body) = text.split_once('\n').ok_or("unsupported manifest format")?;
    if header != MANIFEST_HEADER {
        return Err("unsupported manifest format");
    }
    let mut lsn = None;
    let mut next_rowid = None;
    let mut latest_transaction_id = None;
    let mut catalog_sequence = None;
    let mut role_sequence = None;
    let mut next_large_object_oid = None;
    let mut writer = None;
    let mut writer_range = None;
    let mut ended = false;
    let mut types_finished = false;
    let mut offset = header.len() + 1;
    for line in body.split_inclusive('\n') {
        if ended {
            return Err("trailing manifest records");
        }
        let content = line
            .strip_suffix('\n')
            .ok_or("manifest has no final terminator")?;
        let mut words = content.split(' ');
        let metadata = match words.next() {
            Some("lsn") => {
                if lsn.is_some() {
                    return Err("duplicate manifest LSN");
                }
                lsn = Some(
                    words
                        .next()
                        .and_then(|word| word.parse().ok())
                        .ok_or("bad manifest LSN")?,
                );
                true
            }
            Some("next_rowid") => {
                if next_rowid.is_some() {
                    return Err("duplicate row identity allocator");
                }
                next_rowid = Some(
                    words
                        .next()
                        .and_then(|word| word.parse().ok())
                        .ok_or("invalid row identity allocator")?,
                );
                true
            }
            Some("latest_transaction_id") => {
                if latest_transaction_id.is_some() {
                    return Err("duplicate transaction identity allocator");
                }
                latest_transaction_id = Some(
                    words
                        .next()
                        .and_then(|word| word.parse().ok())
                        .ok_or("invalid transaction identity allocator")?,
                );
                true
            }
            Some("catalog_sequence") => {
                if catalog_sequence.is_some() {
                    return Err("duplicate catalog identity allocator");
                }
                let sequence: u64 = words
                    .next()
                    .and_then(|word| word.parse().ok())
                    .ok_or("invalid catalog identity allocator")?;
                catalog_sequence = Some(sequence);
                true
            }
            Some("role_sequence") => {
                if role_sequence.is_some() {
                    return Err("duplicate role identity allocator");
                }
                let sequence: u64 = words
                    .next()
                    .and_then(|word| word.parse().ok())
                    .ok_or("invalid role identity allocator")?;
                if !(crate::storage::RoleOid::USER_FLOOR..=i32::MAX as u64).contains(&sequence) {
                    return Err("invalid role identity allocator");
                }
                role_sequence = Some(sequence);
                true
            }
            Some("next_lo_oid") => {
                if next_large_object_oid.is_some() {
                    return Err("duplicate large-object allocator");
                }
                let value = words.next().ok_or("large-object allocator missing")?;
                let oid = if value == "-" {
                    None
                } else {
                    Some(
                        value
                            .parse::<u32>()
                            .ok()
                            .and_then(crate::sql::ast::LargeObjectId::parse)
                            .ok_or("invalid large-object allocator")?,
                    )
                };
                next_large_object_oid = Some(oid);
                true
            }
            Some("writer") => {
                if writer.is_some() {
                    return Err("manifest has an invalid writer identity");
                }
                let word = words
                    .next()
                    .filter(|word| {
                        word.len() == 32 && word.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
                    .ok_or("manifest has an invalid writer identity")?;
                writer = Some(
                    u128::from_str_radix(word, 16)
                        .ok()
                        .filter(|token| *token != 0)
                        .map(WriterToken)
                        .ok_or("manifest has an invalid writer identity")?,
                );
                writer_range = Some((offset, offset + content.len()));
                true
            }
            Some("types_end") => {
                if types_finished {
                    return Err("duplicate type catalog boundary");
                }
                types_finished = true;
                true
            }
            Some("dom6" | "enm3" | "cmp2") if types_finished => {
                return Err("type source follows its catalog boundary");
            }
            Some("end") => {
                ended = true;
                true
            }
            _ => false,
        };
        if metadata && words.next().is_some() {
            return Err("trailing manifest metadata fields");
        }
        offset += line.len();
    }
    if !ended {
        return Err("manifest has no final terminator");
    }
    if !types_finished {
        return Err("manifest lacks type catalog boundary");
    }
    Ok(ManifestMetadata {
        lsn: lsn.ok_or("manifest LSN missing")?,
        next_rowid: next_rowid.ok_or("manifest lacks row identity allocator")?,
        latest_transaction_id: latest_transaction_id
            .ok_or("manifest lacks transaction identity allocator")?,
        catalog_sequence: catalog_sequence.ok_or("manifest lacks catalog identity allocator")?,
        role_sequence: role_sequence.ok_or("manifest lacks role identity allocator")?,
        next_large_object_oid: next_large_object_oid
            .ok_or("manifest lacks large-object allocator")?,
        writer: writer.ok_or("manifest writer identity missing")?,
        writer_range: writer_range.ok_or("manifest writer identity missing")?,
    })
}

#[cfg(feature = "checkpoint-profile")]
pub(crate) struct CheckpointProfileStart {
    realtime_ns: u64,
    monotonic: std::time::Instant,
}

#[cfg(feature = "checkpoint-profile")]
fn checkpoint_profile_start() -> CheckpointProfileStart {
    let mut now = core::mem::MaybeUninit::<libc::timespec>::uninit();
    let result = unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, now.as_mut_ptr()) };
    assert_eq!(result, 0, "CLOCK_REALTIME is available");
    let now = unsafe { now.assume_init() };
    CheckpointProfileStart {
        realtime_ns: (now.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(now.tv_nsec as u64),
        monotonic: std::time::Instant::now(),
    }
}

#[cfg(feature = "checkpoint-profile")]
fn profile_checkpoint_phase(
    phase: &'static str,
    lsn: u64,
    slot: Option<usize>,
    started: CheckpointProfileStart,
    before: crate::store::BlockIoStats,
    after: crate::store::BlockIoStats,
    deleted: usize,
) {
    let elapsed_ns = started.monotonic.elapsed().as_nanos().min(u64::MAX as u128) as u64;
    let ended_ns = started.realtime_ns.saturating_add(elapsed_ns);
    let io = after.saturating_sub(before);
    let line = stack_format!(
        320,
        "checkpoint_phase lsn={} phase={} slot={} started_ns={} ended_ns={} elapsed_us={} block_gets={} block_puts={} deleted={}\n",
        lsn,
        phase,
        slot.map_or(-1, |slot| slot as i64),
        started.realtime_ns,
        ended_ns,
        elapsed_ns / 1_000,
        io.object_gets,
        io.object_puts,
        deleted
    );
    // A fixed stack line keeps diagnostics outside the runtime heap budget.
    unsafe {
        libc::write(2, line.as_str().as_ptr().cast(), line.as_str().len());
    }
}

/// A published roster proves these immutable blocks already reached the
/// durable tier. Their content identity lets a rebuild reuse unchanged blocks.
struct PublishedValueBlockWriter<'a> {
    inner: &'a mut dyn BlockWriter,
    known_blocks: &'a [(BlockId, Option<BlockType>)],
}

impl BlockWriter for PublishedValueBlockWriter<'_> {
    fn write_block(
        &mut self,
        payload: &[u8],
        block_type: BlockType,
        lsn: u64,
    ) -> Result<BlockId, StoreError> {
        if lsn == 0 {
            let id = BlockId::for_block(block_type, payload);
            if self
                .known_blocks
                .binary_search(&(id, Some(block_type)))
                .is_ok()
            {
                return Ok(id);
            }
        }
        self.inner.write_block(payload, block_type, lsn)
    }
}

/// io_error — object storage trouble surfaced to a statement.
const SQLSTATE_IO: &str = "58030";
/// serialization_failure — manifest CAS lost to another writer.
const SQLSTATE_CAS: &str = "40001";

fn value_sort_entry_len(entry: &[u8]) -> Result<usize, SqlError> {
    if entry.len() < VALUE_SORT_ENTRY_HEADER {
        return Err(sql_err!(
            SQLSTATE_IO,
            "persistent value-index sort row is truncated"
        ));
    }
    let key_len = u32::from_le_bytes(entry[24..28].try_into().unwrap()) as usize;
    let payload_len = u32::from_le_bytes(entry[28..32].try_into().unwrap()) as usize;
    VALUE_SORT_ENTRY_HEADER
        .checked_add(key_len)
        .and_then(|end| end.checked_add(payload_len))
        .filter(|end| *end <= entry.len())
        .ok_or_else(|| sql_err!(SQLSTATE_IO, "persistent value-index sort row is malformed"))
}

fn value_sort_key(entry: &[u8]) -> Result<&[u8], SqlError> {
    let entry_len = value_sort_entry_len(entry)?;
    if entry_len != entry.len() {
        return Err(sql_err!(
            SQLSTATE_IO,
            "persistent value-index sort row has trailing bytes"
        ));
    }
    let key_len = u32::from_le_bytes(entry[24..28].try_into().unwrap()) as usize;
    Ok(&entry[VALUE_SORT_ENTRY_HEADER..VALUE_SORT_ENTRY_HEADER + key_len])
}

fn value_sort_payload(entry: &[u8]) -> Result<&[u8], SqlError> {
    let key = value_sort_key(entry)?;
    Ok(&entry[VALUE_SORT_ENTRY_HEADER + key.len()..])
}

fn compare_posting_sort_entries(left: &[u8], right: &[u8]) -> Result<Ordering, SqlError> {
    Ok(value_sort_key(left)?
        .cmp(value_sort_key(right)?)
        .then_with(|| left[8..24].cmp(&right[8..24])))
}

/// Identity of one immutable commit batch.  The LSN and checksum always
/// travel together, so a recovered head cannot pair either with another
/// batch's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CommitBatchId {
    first_lsn: u64,
    digest: u32,
}

impl CommitBatchId {
    const EMPTY: Self = Self {
        first_lsn: 0,
        digest: 0,
    };

    fn from_bytes(first_lsn: u64, bytes: &[u8]) -> Self {
        Self {
            first_lsn,
            digest: crate::wal::crc32c::crc32c(bytes),
        }
    }
}

/// A spill-list update awaiting the manifest publish.
#[derive(Clone)]
enum SlotInstall {
    Append {
        reservation: crate::storage::SpillGenerationReservation,
        handle: SstHandle,
        through_lsn: u64,
    },
    Replace {
        reservation: crate::storage::SpillGenerationReservation,
        handle: SstHandle,
        through_lsn: u64,
    },
    Clear {
        target: crate::storage::SpillPublication,
    },
    /// Paced compaction merged the adjacent pair at list positions
    /// (`at`, `at + 1`) into one (`None` when everything in the pair was
    /// deleted). Existing row references retain their generation identities.
    MergePair {
        target: crate::storage::SpillPublication,
        expected: (SstHandle, SstHandle),
        at: usize,
        handle: Option<SstHandle>,
    },
}

/// Physical row layout captured by one table slice. A reslice may retain the
/// earlier immutable delta only when every generation uses the same decoder.
#[derive(Clone, Copy, PartialEq, Eq)]
struct SliceLayout {
    created_at: u64,
    rewrite_generation: u64,
    column_types: [ColType; MAX_COLUMNS],
    n_columns: u16,
    fillfactor: u8,
}

impl SliceLayout {
    const EMPTY: Self = Self {
        created_at: 0,
        rewrite_generation: 0,
        column_types: [ColType::Bool; MAX_COLUMNS],
        n_columns: 0,
        fillfactor: 0,
    };

    fn from_storage(storage: &Storage, slot: usize) -> Self {
        let definition = &*storage.table(slot).definition();
        let mut layout = Self::EMPTY;
        layout.created_at = storage.table(slot).created_at();
        layout.rewrite_generation = storage.table(slot).rewrite_generation();
        layout.n_columns = definition.n_columns as u16;
        layout.fillfactor = definition.storage_options.fillfactor.unwrap_or(0);
        for (target, column) in layout.column_types.iter_mut().zip(definition.columns()) {
            *target = column.ctype;
        }
        layout
    }
}

#[derive(Clone, Copy)]
struct ValueInstall {
    slot: usize,
    table_created_at: u64,
    columns: [u16; crate::storage::MAX_INDEX_COLS],
    n_columns: usize,
    index_created_at: Option<u64>,
    include_mask: crate::storage::ColumnSet,
    dirty_lsn: u64,
    handle: Option<ValueIndexHandle>,
}

#[derive(Clone, Copy)]
enum ValueIndexSource {
    InMemory { rows: usize, position: usize },
    External { run: ExternalRun, started: bool },
}

#[derive(Clone, Copy)]
struct BufferedValueRow {
    offset: u32,
    length: u32,
    ordinal: u64,
}

const EMPTY_BUFFERED_VALUE_ROW: BufferedValueRow = BufferedValueRow {
    offset: 0,
    length: 0,
    ordinal: 0,
};

#[derive(Clone, Copy)]
struct PendingValueRow {
    rowid: u64,
    commit_lsn: u64,
    hash: u64,
    key_len: usize,
    payload_len: usize,
    token_skip: usize,
}

#[derive(Clone, Copy)]
enum ValueMergeDestination {
    Carry(usize),
    Finish(usize),
}

enum ValueSchedulePhase {
    Collect,
    WriteChunk {
        position: usize,
    },
    StartMerge {
        left: ExternalRun,
        right: ExternalRun,
        destination: ValueMergeDestination,
    },
    Merge {
        destination: ValueMergeDestination,
        right_cursor: SstCursor,
        right_len: usize,
        rows: u64,
        output: u64,
    },
    Finish {
        accumulator: Option<ExternalRun>,
        level: usize,
    },
    CompleteRun {
        run: ExternalRun,
        destination: ValueMergeDestination,
    },
}

struct ValueScheduleJob {
    slot: usize,
    table_created_at: u64,
    table_generation: u64,
    binding: usize,
    columns: [u16; crate::storage::MAX_INDEX_COLS],
    n_columns: usize,
    index_created_at: Option<u64>,
    include_mask: crate::storage::ColumnSet,
    navigation: Option<crate::store::NavigationSpec>,
    dirty_lsn: u64,
    through_lsn: u64,
    base: Option<ValueIndexHandle>,
    phase: ValueSchedulePhase,
    source_done: bool,
    pending: Option<PendingValueRow>,
    chunk_len: usize,
    row_count: usize,
    next_ordinal: u64,
    levels: [Option<ExternalRun>; 64],
}

#[derive(Clone, Copy)]
enum ValueIndexPhase {
    Write,
    Finish(Option<ValueIndexHandle>),
}

#[derive(Clone, Copy)]
enum IndexOutputOwner {
    Schedule,
    Index,
}

struct ValueIndexJob {
    phase: ValueIndexPhase,
    slot: usize,
    table_created_at: u64,
    binding: usize,
    columns: [u16; crate::storage::MAX_INDEX_COLS],
    n_columns: usize,
    index_created_at: Option<u64>,
    include_mask: crate::storage::ColumnSet,
    navigation: Option<crate::store::NavigationSpec>,
    dirty_lsn: u64,
    through_lsn: u64,
    base: Option<ValueIndexHandle>,
    base_len: usize,
    base_done: bool,
    source: ValueIndexSource,
    previous_posting: Option<(
        [u8; crate::sql::index_signature::PostingToken::KEY_BYTES],
        u64,
        u64,
    )>,
}

type LoadedValueIndex = (
    usize,
    [u16; crate::storage::MAX_INDEX_COLS],
    usize,
    Option<u64>,
    crate::storage::ColumnSet,
    ValueIndexHandle,
);

#[derive(Clone, Copy)]
struct CheckpointVersion {
    key: SstKey,
    home: Option<RowHome>,
}

enum TableSlicePhase {
    Clean,
    Write {
        source: crate::storage::TableRowSnapshot<'static>,
        position: usize,
    },
    Finish(Option<SstHandle>),
}

struct TableSliceJob {
    slot: usize,
    layout: SliceLayout,
    generation: u64,
    through_lsn: u64,
    reuse_slice: bool,
    delta: bool,
    count: u64,
    crc: Crc32c,
    installs: [Option<SlotInstall>; 2],
    install_count: usize,
    phase: TableSlicePhase,
}

/// A prior checkpoint's SST reference for one table slot.
#[derive(Clone, Copy)]
struct PrevSst {
    handle: SstHandle,
    count: u64,
    crc: u32,
}

/// One table's published SST list. Its backing slice is reserved at startup;
/// checkpoint publication only mutates entries in place.
struct SlotList {
    ssts: Box<[Option<PrevSst>]>,
    n: usize,
}

impl SlotList {
    fn new(capacity: usize) -> Self {
        Self {
            ssts: vec![None; capacity].into_boxed_slice(),
            n: 0,
        }
    }

    fn clear(&mut self) {
        self.ssts.fill(None);
        self.n = 0;
    }

    fn copy_from(&mut self, source: &Self) {
        debug_assert_eq!(self.ssts.len(), source.ssts.len());
        self.ssts.copy_from_slice(&source.ssts);
        self.n = source.n;
    }

    fn capacity(&self) -> usize {
        self.ssts.len()
    }

    fn replace_pair(&mut self, at: usize, merged: Option<PrevSst>) {
        debug_assert!(at + 1 < self.n);
        let removed = if merged.is_some() { 1 } else { 2 };
        let mut write = at;
        if let Some(member) = merged {
            self.ssts[write] = Some(member);
            write += 1;
        }
        for read in at + 2..self.n {
            self.ssts[write] = self.ssts[read];
            write += 1;
        }
        self.n -= removed;
        self.ssts[self.n..].fill(None);
    }

    fn push(&mut self, p: PrevSst) -> bool {
        if self.n == self.ssts.len() {
            return false;
        }
        self.ssts[self.n] = Some(p);
        self.n += 1;
        true
    }

    fn iter(&self) -> impl Iterator<Item = &PrevSst> {
        self.ssts[..self.n].iter().filter_map(|p| p.as_ref())
    }
}

/// Where a paced merge stands between beats.
enum MergePhase {
    /// Building the id schedule: member `rank`'s scan resumes at `resume_lo`.
    Schedule { rank: u8, resume_lo: SstKey },
    /// Streaming scheduled entries into the merged SST from `cursor`.
    Write { cursor: usize },
    /// The handle is private until its last output emission is durable.
    Finish { merged: Option<PrevSst> },
}

/// A merge in flight across beats: which pair of which table's list, and
/// the accumulated output bookkeeping. The half-written SST itself lives in
/// the checkpointer's dedicated merge writer.
struct MergeJob {
    slot: usize,
    at: usize,
    old0: PrevSst,
    old1: PrevSst,
    /// True at the list head: nothing older remains for a tombstone to
    /// suppress, so none survives the merge.
    drop_tombstones: bool,
    /// Compaction keeps every version newer than this watermark and the first
    /// version at or below it. None means only the current image is needed.
    oldest_snapshot: Option<u64>,
    phase: MergePhase,
    schedule_len: usize,
    count: u64,
    crc: Crc32c,
}

/// A finished merge awaiting the next publish, which composes it into the
/// slot's list — or discards it if a collapse superseded the pair.
#[derive(Clone, Copy)]
struct CompletedMerge {
    at: usize,
    old0: PrevSst,
    old1: PrevSst,
    merged: Option<PrevSst>,
}

/// One merge beat's verdict.
enum MergeBeatOutcome {
    Continue,
    Cancel,
    Finished(Option<PrevSst>),
}

/// The adjacent pair's handles at position `at`, if both exist.
fn pair_at(list: &SlotList, at: usize) -> Option<(SstHandle, SstHandle)> {
    let a = list.ssts.get(at).copied().flatten()?;
    let b = list.ssts.get(at + 1).copied().flatten()?;
    Some((a.handle, b.handle))
}

fn push_slot_list(list: &mut SlotList, prior: PrevSst) -> Result<(), SqlError> {
    if !list.push(prior) {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "spill list exceeds max_spill_generations_per_table ({})",
            list.capacity()
        ));
    }
    Ok(())
}

fn garbage_sst_root_capacity(config: &Config) -> usize {
    crate::storage::Storage::retained_spill_root_capacity(config)
        + crate::storage::table_slot_capacity(config)
            .saturating_mul(config.max_spill_generations_per_table.saturating_add(1))
}

fn garbage_index_root_capacity(config: &Config) -> usize {
    crate::storage::Storage::retained_index_file_capacity(config)
        .saturating_add(config.max_value_indexes)
}

pub(crate) struct TemporarySpiller {
    blocks: std::sync::Arc<Mutex<crate::store::EphemeralBlockStore>>,
    handles: Vec<SstHandle>,
    sst_arena: Arena,
    writer: SstWriter,
}

impl TemporarySpiller {
    pub(crate) fn budget_bytes(config: &Config) -> usize {
        crate::store::EphemeralBlockStore::budget_bytes(config.temporary_spill_bytes)
            + garbage_sst_root_capacity(config) * core::mem::size_of::<SstHandle>()
            + SST_ARENA_BYTES
            + SstWriter::budget_bytes()
    }

    pub(crate) fn new(config: &Config, budget: &mut Budget) -> Result<Self, CheckpointSetupError> {
        std::fs::create_dir_all(&config.data_dir).map_err(|error| {
            CheckpointSetupError::ObjectStore(format!("create data_dir: {error}"))
        })?;
        let path = std::path::Path::new(&config.data_dir).join("temporary-spill");
        let blocks =
            crate::store::EphemeralBlockStore::open(budget, &path, config.temporary_spill_bytes)
                .map_err(|error| {
                    CheckpointSetupError::ObjectStore(format!("temporary spill store: {error}"))
                })?;
        budget
            .draw(SstWriter::budget_bytes(), "temporary spill writer")
            .map_err(CheckpointSetupError::Budget)?;
        budget
            .draw(
                garbage_sst_root_capacity(config) * core::mem::size_of::<SstHandle>(),
                "temporary spill retained root roster",
            )
            .map_err(CheckpointSetupError::Budget)?;
        Ok(Self {
            blocks: std::sync::Arc::new(Mutex::new(blocks)),
            handles: Vec::with_capacity(garbage_sst_root_capacity(config)),
            sst_arena: Arena::new(budget, "temporary spill sst", SST_ARENA_BYTES)
                .map_err(CheckpointSetupError::Budget)?,
            writer: SstWriter::new(),
        })
    }

    pub(crate) fn block_store(&self) -> std::sync::Arc<Mutex<crate::store::EphemeralBlockStore>> {
        std::sync::Arc::clone(&self.blocks)
    }

    /// Flushes one dirty temporary table into the process-local block store.
    /// This deliberately has no manifest phase: installing the handle and
    /// evicting its heap rows is safe only for the current process, and a
    /// restart discards both the temporary catalog and this file.
    pub(crate) fn spill_step(
        &mut self,
        storage: &mut Storage,
        sort_scratch: &mut FixedVec<(u64, RowHome)>,
    ) -> Result<Option<usize>, SqlError> {
        self.reclaim(storage)?;
        let Some(slot) = (0..storage.physical_table_count()).find(|&slot| {
            let table = storage.table(slot);
            table.live()
                && {
                    let definition = table.definition();
                    definition.persistence
                } == crate::storage::RelationPersistence::Temporary
                && table.dirty()
        }) else {
            return Ok(None);
        };
        let generation = storage.table(slot).generation();
        let delta = {
            let generations = storage.table(slot).spill_generations();
            !generations.is_empty() && generations.len() < generations.capacity()
        };

        sort_scratch.clear();
        storage.for_each_row_state(slot, &mut |rowid, state| {
            use core::ops::ControlFlow;
            let has_version = state.has_committed_version() || !state.history.is_empty();
            if !has_version {
                return Ok(ControlFlow::Continue(()));
            }
            let resident = matches!(state.committed, Some(RowHome::Heap(_)))
                || (state.committed.is_none() && state.has_committed_version())
                || (0..state.history.len()).any(|index| {
                    state.history_get(index).is_some_and(|version| {
                        version.home.is_none() || matches!(version.home, Some(RowHome::Heap(_)))
                    })
                });
            if delta && !resident {
                return Ok(ControlFlow::Continue(()));
            }
            let marker = state
                .committed
                .or_else(|| {
                    (0..state.history.len()).find_map(|index| state.history_get(index)?.home)
                })
                .unwrap_or(RowHome::Heap(crate::storage::RowLoc::EMPTY));
            sort_scratch.push((rowid, marker)).map_err(|error| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "temporary spill scratch: {}",
                    error
                )
            })?;
            Ok(ControlFlow::Continue(()))
        })?;
        sort_scratch
            .as_mut_slice()
            .sort_unstable_by_key(|(rowid, _)| *rowid);

        self.sst_arena.reset();
        self.writer.reset();
        let mut schema = [ColType::Bool; MAX_COLUMNS];
        let columns = storage.table(slot).definition().schema(&mut schema);
        self.writer
            .set_pax_schema(&schema[..columns])
            .map_err(temporary_sst_to_sql)?;
        self.writer
            .set_packed_fillfactor(storage.table(slot).definition().storage_options.fillfactor)
            .map_err(temporary_sst_to_sql)?;
        let writer = &mut self.writer;
        let blocks = &self.blocks;
        for &(rowid, _) in sort_scratch.iter() {
            let Some(state) = storage.row_state(slot, rowid)? else {
                continue;
            };
            let mut append_version =
                |commit_lsn: u64, home: Option<RowHome>| -> Result<(), SqlError> {
                    if delta && home.is_some_and(|location| !matches!(location, RowHome::Heap(_))) {
                        return Ok(());
                    }
                    let key = SstKey::at(rowid, commit_lsn);
                    if let Some(location) = home {
                        storage.with_row_bytes(slot, rowid, location, |row| {
                            writer
                                .append_version(
                                    &mut *blocks.lock().expect("block store lock poisoned"),
                                    key,
                                    row,
                                )
                                .map_err(temporary_sst_to_sql)
                        })?
                    } else {
                        writer
                            .append_tombstone_version(
                                &mut *blocks.lock().expect("block store lock poisoned"),
                                key,
                            )
                            .map_err(temporary_sst_to_sql)?
                    }
                    Ok(())
                };
            if state.has_committed_version() {
                append_version(state.committed_lsn, state.committed)?;
            }
            for index in 0..state.history.len() {
                if let Some(version) = state.history_get(index) {
                    append_version(version.lsn, version.home)?;
                }
            }
        }
        let handle = writer
            .finish(&mut *blocks.lock().expect("block store lock poisoned"))
            .map_err(temporary_sst_to_sql)?;
        match (delta, handle) {
            (true, Some(handle)) => {
                storage.install_temporary_spill(slot, handle, generation, true)?
            }
            (true, None) => {}
            (false, Some(handle)) => {
                storage.install_temporary_spill(slot, handle, generation, false)?
            }
            (false, None) => storage.set_spill_list(slot, &[]),
        }
        storage.clear_table_dirty_at_generation(slot, generation);
        self.reclaim(storage)?;
        Ok(Some(slot))
    }

    #[cfg(test)]
    pub(crate) fn block_count(&self) -> usize {
        self.blocks
            .lock()
            .expect("block store lock poisoned")
            .block_count()
    }

    pub(crate) fn cleanup(&mut self, storage: &Storage) {
        if let Err(error) = self.reclaim(storage) {
            let message = stack_format!(
                512,
                "temporary spill cleanup failed ({}): {}\n",
                error.sqlstate,
                error.message.as_str()
            );
            crate::util::stderr_line(message.as_str());
        }
    }

    fn reclaim(&mut self, storage: &Storage) -> Result<(), SqlError> {
        storage.copy_retained_spill_roots(false, &mut self.handles)?;
        for slot in 0..storage.physical_table_count() {
            let table = storage.table(slot);
            if !table.live() || {
                let definition = table.definition();
                definition.persistence
            } != crate::storage::RelationPersistence::Temporary
            {
                continue;
            }
            let generations = table.spill_generations();
            for member in generations.iter() {
                if self.handles.len() == self.handles.capacity() {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "temporary spill handle roster exceeds its fixed capacity"
                    ));
                }
                self.handles.push(member.handle);
            }
        }
        self.handles.sort_unstable_by_key(|handle| handle.roster);
        self.handles.dedup_by_key(|handle| handle.roster);
        if self.handles.is_empty() {
            self.blocks
                .lock()
                .expect("block store lock poisoned")
                .clear();
            return Ok(());
        }
        self.blocks
            .lock()
            .expect("temporary block store lock poisoned")
            .retain(&self.handles)
            .map_err(temporary_store_to_sql)
    }
}

#[derive(Clone, Copy)]
#[allow(
    clippy::large_enum_variant,
    reason = "bounded page cursor is startup-reserved"
)]
enum CommitPruneProgress {
    Unloaded,
    Backups {
        cursor: ListCursor,
    },
    BackupFloors {
        index: usize,
        oldest: Option<u64>,
    },
    FindBoundary {
        floor: u64,
        cursor: Option<CommitBatchId>,
    },
    List {
        floor: u64,
        boundary: Option<CommitBatchId>,
        cursor: ListCursor,
    },
    Delete {
        floor: u64,
        boundary: Option<CommitBatchId>,
        overflow: bool,
    },
}

/// Only names emitted by the journal writer can enter the fixed deletion queue.
fn commit_object_identity(key: &str) -> Option<CommitBatchId> {
    let name = key.strip_prefix("commits/")?;
    let stem = name
        .strip_suffix(".head")
        .or_else(|| name.strip_suffix(".batch"))?;
    let bytes = stem.as_bytes();
    if bytes.len() != 29
        || bytes[20] != b'-'
        || !bytes[..20].iter().all(u8::is_ascii_digit)
        || !bytes[21..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        return None;
    }
    Some(CommitBatchId {
        first_lsn: stem[..20].parse().ok()?,
        digest: u32::from_str_radix(&stem[21..], 16).ok()?,
    })
}

/// Restores the caller's mode after errors and nested atomic execution.
#[must_use]
pub(crate) struct AtomicBlockReadScope {
    blocks: std::sync::Arc<Mutex<TieredStore<OwnedObjectStore>>>,
    active: core::cell::Cell<bool>,
}

impl AtomicBlockReadScope {
    pub(crate) fn activate(&self) {
        if !self.active.replace(true) {
            self.blocks
                .lock()
                .expect("block store lock poisoned")
                .base_mut()
                .enter_atomic_reads();
        }
    }
}

impl Drop for AtomicBlockReadScope {
    fn drop(&mut self) {
        if self.active.get() {
            self.blocks
                .lock()
                .expect("block store lock poisoned")
                .base_mut()
                .leave_atomic_reads();
        }
    }
}

enum CommitPublication {
    Objects {
        batch: CommitBatchId,
        data: Option<crate::object_store::upload::Ticket>,
        descriptor: Option<crate::object_store::upload::Ticket>,
    },
    Root {
        batch: CommitBatchId,
        ticket: crate::object_store::upload::Ticket,
    },
}

pub(crate) const MANIFEST_UPLOAD_SLOTS: usize = 1;
pub(crate) const CREDENTIAL_UPLOAD_SLOTS: usize = 1;
const CREDENTIAL_UPLOAD_BYTES: usize = 128;

pub(crate) enum CredentialReloadStep {
    Pending,
    Installed,
}

enum CredentialRenewalPhase {
    Uploading(crate::object_store::upload::Ticket),
    Installing(EntityTag),
}

struct CredentialRenewal {
    credentials: crate::object_store::Credentials,
    phase: CredentialRenewalPhase,
}

#[derive(Clone, Copy)]
pub(crate) struct CheckpointRequest(core::num::NonZeroU64);

impl CheckpointRequest {
    pub(crate) fn epoch(self) -> u64 {
        self.0.get()
    }
}

enum ManifestPublication {
    Uploading {
        lsn: u64,
        ticket: crate::object_store::upload::Ticket,
    },
    Retry {
        lsn: u64,
        at: std::time::Instant,
    },
    Installing {
        lsn: u64,
        row_position: usize,
        value_position: usize,
        cleanup_position: usize,
        cleanup_rows: Option<RowCleanup>,
    },
}

// One append may flush a prior group and a maximum chained row; finish may
// flush column frames, index levels, filter and roster. Index navigation adds
// its fixed-depth frontier. This reserves one operation, independent of rows.
const PHYSICAL_EMISSION_BYTES: usize = crate::store::MAX_ASSEMBLED
    + 8 * crate::store::BLOCK_SIZE
    + MAX_COLUMNS * (crate::store::HEADER_LEN + 8 + 7)
    + ValueIndexWriter::budget_bytes();
const PHYSICAL_EMISSION_OBJECTS: usize = MAX_COLUMNS + 32;

pub(crate) struct Checkpointer {
    pub(crate) client: ObjectStore,
    metadata_io: MetadataIo,
    uploads: crate::object_store::upload::Uploads,
    block_uploads: crate::object_store::upload::Uploads,
    manifest_uploads: crate::object_store::upload::Uploads,
    writer_probe: Option<WriterProbe>,
    credential_uploads: Option<crate::object_store::upload::Uploads>,
    credential_renewal: Option<CredentialRenewal>,
    manifest_publication: Option<ManifestPublication>,
    requested_checkpoint_epoch: u64,
    captured_checkpoint_epoch: u64,
    published_checkpoint_epoch: u64,
    block_output: crate::store::BlockWriteBatch,
    index_output_owner: Option<IndexOutputOwner>,
    table_output: FixedVec<CheckpointVersion>,
    table_job: Option<TableSliceJob>,
    commit_publication: Option<CommitPublication>,
    /// Immutable table/index blocks through the startup-sized RAM/disk caches.
    /// Storage's spilled-row reader shares this stack. Pending object requests
    /// own their buffers after the short metadata guard ends.
    blocks: std::sync::Arc<Mutex<TieredStore<OwnedObjectStore>>>,
    /// Scratch shared by checkpoint, merge and garbage-collection turns.
    sst_arena: Arena,
    parameter_acl_arena: Arena,
    /// Spill-list updates computed during a checkpoint, applied to storage
    /// only after the manifest CAS lands.
    pending_installs: Vec<(usize, SlotInstall)>,
    pending_value_installs: Vec<ValueInstall>,
    /// Pre-reserved physical-version schedule: (key, source-and-kind).
    merge_scratch: Vec<(SstKey, u8)>,
    /// Published index identities during a rebuild; untyped block identities
    /// during garbage collection. Both uses fit checkpoint_live_blocks.
    roster_scratch: Vec<(BlockId, Option<BlockType>)>,
    garbage_seen: FixedMap<BlockId, ()>,
    garbage_sst_roots: Vec<SstHandle>,
    garbage_index_roots: Vec<BlockId>,
    doomed_blocks: Vec<StackStr<80>>,
    manifest_buf: FixedBuf,
    manifest_output: FixedBuf,
    prepared_transaction_snapshot: FixedVec<crate::storage::PreparedTransactionCatalogEntry>,
    manifest_etag: Option<EntityTag>,
    manifest_present: bool,
    manifest_lsn: u64,
    /// CAS-published root of the immutable commit-batch chain.  A batch PUT
    /// alone is intentionally not recoverable until this pointer advances.
    commit_head_etag: Option<EntityTag>,
    commit_head: Option<CommitBatchId>,
    root_buf: FixedBuf,
    /// Per-slot SST from the last published manifest; clean tables reuse
    /// these handles (delta checkpoints). Capacity is reserved at startup so
    /// the post-freeze checkpoint path never allocates.
    prev_ssts: Vec<SlotList>,
    /// Pre-reserved scratch built during a checkpoint, then swapped into the
    /// fields above; keeps the post-freeze path allocation-free.
    prev_scratch: Vec<SlotList>,
    /// One table's next slice, committed into `prev_scratch` only after every
    /// fallible row and value-index write succeeds.
    slice_scratch: SlotList,
    /// Pre-reserved scratch for cold commit replay and object deletion batches.
    commit_scratch: Vec<StackStr<64>>,
    backup_scratch: Vec<StackStr<BACKUP_NAME_BYTES>>,
    backup_roster_loaded: bool,
    garbage_scratch: Vec<StackStr<64>>,
    prune_backup_scratch: Vec<StackStr<BACKUP_NAME_BYTES>>,
    prune_backup_roster_loaded: bool,
    commit_garbage_scratch: Vec<StackStr<64>>,
    delete_objects_per_beat: usize,
    commit_prune: CommitPruneProgress,
    legacy_garbage_loaded: bool,
    legacy_garbage_overflow: bool,
    block_garbage_loaded: bool,
    block_garbage_overflow: bool,
    /// Sliced-checkpoint sweep state: whether a sweep is mid-flight, the
    /// table generation each slot's slice captured, and which slots were
    /// sliced this sweep.
    sweeping: bool,
    /// A manifest is durable but its bounded garbage sweep still needs a
    /// retry. Its LSN is withheld until maintenance completes.
    published_lsn_pending_maintenance: Option<u64>,
    /// Durable-WAL boundary awaiting paced commit-object deletion.
    commit_prune_through: Option<u64>,
    commit_prune_next: Option<u64>,
    legacy_garbage_pending: bool,
    block_garbage_pending: bool,
    sliced_generation: Vec<u64>,
    /// WAL boundary and physical decoder captured by each current slice.
    /// Compatible re-slices append only versions committed after this LSN.
    sliced_lsn: Vec<u64>,
    sliced_layout: Vec<SliceLayout>,
    sliced_this_sweep: Vec<bool>,
    checkpoint_deletions: Vec<Option<std::sync::Arc<()>>>,
    checkpoint_statistics: Vec<Option<crate::storage::CheckpointStatistics>>,
    /// The slice writer (reset per table) and the merge writer, which holds
    /// a half-written SST across beats — the reason the writer owns its
    /// state instead of borrowing an arena.
    slice_writer: SstWriter,
    merge_writer: SstWriter,
    /// The two merge inputs retain their physical position and decoded data
    /// block across beats. Transient index/PAX decode buffers are shared
    /// because only one source advances at a time.
    merge_source_cursors: [Option<SstVersionCursor>; 2],
    merge_source_decoded: Box<[u8]>,
    merge_source_index: Box<[u8]>,
    merge_source_raw: Box<[u8]>,
    merge_source_column: Box<[u8]>,
    merge_row: Box<[u8]>,
    value_writer: ValueIndexWriter,
    /// One value-index rebuild spans dispatch beats. An in-memory sort is
    /// detached into checkpoint-owned bytes; a spilled sort retains its
    /// immutable run, so the leased producer is free for foreground work.
    value_source: Box<[u8]>,
    value_sort_rows: Box<[BufferedValueRow]>,
    value_source_cursor: CheckpointValueCursor,
    value_sort_reader: ExternalRunReader,
    value_changed_rowids: Vec<u64>,
    value_base_stream: ValueIndexStream,
    value_base_entry: Box<[u8]>,
    value_entry: Box<[u8]>,
    value_schedule_job: Option<ValueScheduleJob>,
    value_job: Option<ValueIndexJob>,
    merge_job: Option<MergeJob>,
    /// Finished merges waiting for one manifest publish. One slot per table
    /// lets every dirty filled roster free a generation before the sweep;
    /// otherwise an unrelated completed merge could force the next table
    /// back through a full rewrite.
    merge_done: Vec<Option<CompletedMerge>>,
    /// Fairness toggle: merge beats and sweep beats alternate when both
    /// want the engine, so neither starves the other.
    merge_turn: bool,
    /// Pairs whose scans overflowed the id scratch (their stored counts
    /// under-reported); remembered per slot so the scheduler stops
    /// proposing a merge that cannot be scheduled.
    merge_overflow: Vec<Option<(BlockId, BlockId)>>,
    /// A fresh process incarnation. The durable writer fence and both mutable
    /// roots carry this token, so an ambiguous response cannot be mistaken
    /// for a displaced process's write.
    writer_token: WriterToken,
    writer_fence_etag: Option<EntityTag>,
    writer_fenced: bool,
}

/// One beat's outcome: nothing to publish, a slice written, or the manifest
/// published at `lsn`.
pub(crate) enum CheckpointStep {
    Idle,
    Working,
    Published { lsn: u64 },
}

/// Scratch for one SST writer or reader: the writer's pending block, index and
/// filter, or the reader's index/data/assembly blocks — reset per table.
/// Sized for a reader and a writer living together (a paced merge streams
/// one SST pair through both) plus an assembled-row bounce buffer.
const SST_ARENA_BYTES: usize = 16 * 1024 * 1024;

/// A table whose spill list reaches this many SSTs gets its two oldest
/// members merged during the checkpoint — one bounded merge per table per
/// cycle, so read fan-out stays low without the monolithic full rewrite that
/// a filled list used to force.
const MERGE_TRIGGER: usize = 4;

/// How far one merge beat may go — the pause a beat inserts between
/// statements is a handful of block transfers, never a whole pair. Data
/// blocks *read* per schedule beat, object reads and data blocks *written*
/// per write beat, and a cheap-entry cap so a tombstone-heavy stretch (which
/// emits no blocks) still bounds its walking and checksum work. One row can
/// cross the read budget because its PAX columns are one indivisible image.
const MERGE_SCHEDULE_BEAT_BLOCKS: usize = 8;
const MERGE_WRITE_BEAT_BLOCKS: usize = 4;
const MERGE_WRITE_BEAT_READS: u64 = 8;
const MERGE_BEAT_ENTRIES: usize = 64 * 1024;
pub(crate) const MERGE_SOURCE_SCRATCH_BYTES: usize = 5 * crate::store::MAX_PAYLOAD + MAX_ASSEMBLED;
const VALUE_INDEX_WRITE_BEAT_READS: u64 = 8;
const VALUE_INDEX_WRITE_BEAT_ENTRIES: usize = 1024;
const VALUE_INDEX_SCHEDULE_BEAT_ROWS: usize = 1024;
const VALUE_INDEX_SCHEDULE_BEAT_READS: u64 = 8;
const VALUE_SORT_ROWS_PER_CHUNK: usize = 8192;

impl Checkpointer {
    #[cfg(feature = "checkpoint-profile")]
    pub(crate) fn profile_start(&self) -> (CheckpointProfileStart, crate::store::BlockIoStats) {
        (
            checkpoint_profile_start(),
            self.blocks
                .lock()
                .expect("block store lock poisoned")
                .io_stats(),
        )
    }

    #[cfg(feature = "checkpoint-profile")]
    pub(crate) fn profile_phase(
        &self,
        phase: &'static str,
        lsn: u64,
        started: CheckpointProfileStart,
        before: crate::store::BlockIoStats,
    ) {
        profile_checkpoint_phase(
            phase,
            lsn,
            None,
            started,
            before,
            self.blocks
                .lock()
                .expect("block store lock poisoned")
                .io_stats(),
            0,
        );
    }

    pub(crate) fn workspace_budget_bytes(config: &Config) -> usize {
        let table_capacity = crate::storage::table_slot_capacity(config);
        let table_bookkeeping = table_capacity
            * (2 * core::mem::size_of::<SlotList>()
                + 2 * core::mem::size_of::<u64>()
                + core::mem::size_of::<SliceLayout>()
                + core::mem::size_of::<bool>()
                + core::mem::size_of::<Option<std::sync::Arc<()>>>()
                + core::mem::size_of::<Option<crate::storage::CheckpointStatistics>>()
                + core::mem::size_of::<Option<CompletedMerge>>()
                + core::mem::size_of::<Option<(BlockId, BlockId)>>())
            + table_capacity
                .saturating_mul(config.max_spill_generations_per_table)
                .saturating_mul(core::mem::size_of::<(usize, SlotInstall)>());
        core::mem::size_of::<CommitPruneProgress>()
            + core::mem::size_of::<Option<u64>>()
            + 2 * SstWriter::budget_bytes()
            + ValueIndexWriter::budget_bytes()
            + table_bookkeeping
            + table_capacity.saturating_mul(crate::storage::MAX_VALUE_ENFORCERS)
                * core::mem::size_of::<ValueInstall>()
            + config.checkpoint_live_blocks * core::mem::size_of::<(BlockId, Option<BlockType>)>()
            + config.checkpoint_garbage_batch_objects
                * (core::mem::size_of::<StackStr<80>>() + 2 * core::mem::size_of::<StackStr<64>>())
            + config.checkpoint_commit_batches * core::mem::size_of::<StackStr<64>>()
            + 2 * config.max_backups * core::mem::size_of::<StackStr<BACKUP_NAME_BYTES>>()
            + config.checkpoint_merge_entries * core::mem::size_of::<(SstKey, u8)>()
    }

    pub(crate) fn credential_output_budget_bytes(config: &Config) -> usize {
        if config.object_store_credentials_file.is_empty() {
            0
        } else {
            crate::object_store::upload::Uploads::budget_bytes(
                config,
                CREDENTIAL_UPLOAD_SLOTS,
                CREDENTIAL_UPLOAD_BYTES,
            )
        }
    }

    pub(crate) fn writer_probe_budget_bytes(config: &Config) -> usize {
        if config.operations_listen_addr.is_empty() {
            0
        } else {
            WriterProbe::budget_bytes(config)
        }
    }

    pub(crate) fn manifest_output_budget_bytes(config: &Config) -> usize {
        crate::object_store::upload::Uploads::budget_bytes(
            config,
            MANIFEST_UPLOAD_SLOTS,
            config.checkpoint_manifest_bytes,
        ) + config.checkpoint_manifest_bytes
            + core::mem::size_of::<Option<ManifestPublication>>()
    }

    pub(crate) fn physical_output_budget_bytes(config: &Config) -> usize {
        crate::object_store::upload::Uploads::budget_bytes(
            config,
            config.object_store_block_put_slots,
            crate::store::BLOCK_SIZE,
        ) + crate::store::BlockWriteBatch::budget_bytes(
            PHYSICAL_EMISSION_BYTES,
            PHYSICAL_EMISSION_OBJECTS,
        ) + config
            .checkpoint_write_entries
            .saturating_mul(core::mem::size_of::<CheckpointVersion>())
    }

    pub(crate) fn budget_bytes(config: &Config) -> usize {
        let table_capacity = crate::storage::table_slot_capacity(config);
        // Administrative and background metadata requests have independent
        // owners. Cache tiers draw their own startup budget.
        ObjectStore::budget_bytes(config)
            + MetadataIo::budget_bytes(config)
            + crate::object_store::upload::Uploads::budget_bytes(
                config,
                config.object_store_put_slots,
                config.wal_upload_buffer_bytes,
            )
            + Self::physical_output_budget_bytes(config)
            + Self::manifest_output_budget_bytes(config)
            + Self::writer_probe_budget_bytes(config)
            + Self::credential_output_budget_bytes(config)
            + OwnedObjectStore::budget_bytes(config)
            + Self::workspace_budget_bytes(config)
            + Storage::parameter_acl_image_budget_bytes(config)
            + FixedMap::<BlockId, ()>::budget_bytes(config.checkpoint_live_blocks)
            + ExternalRunReader::budget_bytes()
            + ValueIndexStream::budget_bytes(config.checkpoint_live_blocks)
            + crate::store::MAX_PAYLOAD
            + 2 * crate::store::MAX_PAYLOAD
            + VALUE_SORT_ROWS_PER_CHUNK * core::mem::size_of::<BufferedValueRow>()
            + garbage_sst_root_capacity(config) * core::mem::size_of::<SstHandle>()
            + garbage_index_root_capacity(config) * core::mem::size_of::<BlockId>()
            + CheckpointValueCursor::budget_bytes(
                config.max_spill_generations_per_table,
                config.table_rows,
            )
            + config.table_rows * core::mem::size_of::<u64>()
            + config.max_prepared_transactions
                * core::mem::size_of::<crate::storage::PreparedTransactionCatalogEntry>()
            + (2usize.saturating_mul(table_capacity) + 1)
                .saturating_mul(config.max_spill_generations_per_table)
                .saturating_mul(core::mem::size_of::<Option<PrevSst>>())
            + config.checkpoint_manifest_bytes
            + 128
            + SST_ARENA_BYTES
            + MERGE_SOURCE_SCRATCH_BYTES
    }

    fn reset_merge_writer(&mut self, storage: &Storage, slot: usize) -> Result<(), SqlError> {
        self.merge_writer.reset();
        let mut schema = [ColType::Bool; MAX_COLUMNS];
        let definition = storage.table(slot).definition();
        let columns = definition.schema(&mut schema);
        self.merge_writer
            .set_pax_schema(&schema[..columns])
            .map_err(sst_to_sql)?;
        self.merge_writer
            .set_packed_fillfactor(definition.storage_options.fillfactor)
            .map_err(sst_to_sql)
    }

    /// One bounded step of the paced merge — the compaction work a beat may
    /// do between statements. Starting a job, advancing its schedule scan a
    /// few blocks, streaming a few output blocks, or finishing: each is one
    /// beat, so a pair of any size merges without ever pausing the engine
    /// for more than a handful of block transfers.
    ///
    /// A job survives publishes (its pair's list positions are stable under
    /// delta appends, which only extend the tail) and is dropped when a
    /// relation replacement supersedes the pair — its blocks sweep as
    /// orphans. A crash loses only the job's progress, never data.
    fn merge_beat(&mut self, storage: &Storage) -> Result<(), SqlError> {
        let Some(mut job) = self.merge_job.take() else {
            if let Some(job) = self.merge_candidate(storage) {
                self.merge_scratch.clear();
                self.reset_merge_writer(storage, job.slot)?;
                self.merge_source_cursors = [
                    Some(SstVersionCursor::new(job.old0.handle)),
                    Some(SstVersionCursor::new(job.old1.handle)),
                ];
                self.merge_job = Some(job);
            }
            return Ok(());
        };
        // The published list must still hold the pair where the job left
        // it; a relation replacement also replaces the merge.
        let valid = self
            .prev_ssts
            .get(job.slot)
            .is_some_and(|list| pair_at(list, job.at) == Some((job.old0.handle, job.old1.handle)));
        if !valid {
            self.block_output
                .discard(&mut self.block_uploads)
                .map_err(upload_to_sql)?;
            self.merge_writer.reset();
            return Ok(());
        }
        #[cfg(feature = "checkpoint-profile")]
        let (phase, started, before) = (
            match &job.phase {
                MergePhase::Schedule { .. } => "row_merge_schedule",
                MergePhase::Write { .. } => "row_merge_write",
                MergePhase::Finish { .. } => "row_merge_finish",
            },
            checkpoint_profile_start(),
            self.blocks
                .lock()
                .expect("block store lock poisoned")
                .io_stats(),
        );
        let outcome = match job.phase {
            MergePhase::Schedule { rank, resume_lo } => {
                self.merge_schedule_beat(&mut job, rank, resume_lo)
            }
            MergePhase::Write { cursor } => self.merge_write_beat(&mut job, cursor),
            MergePhase::Finish { merged } => Ok(MergeBeatOutcome::Finished(merged)),
        };
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                // A read can fail after cursors, checksum or writer state changed.
                // Retry the same immutable pair with one coherent empty output.
                self.block_output
                    .discard(&mut self.block_uploads)
                    .map_err(upload_to_sql)?;
                let reset = self.reset_merge_writer(storage, job.slot);
                self.merge_scratch.clear();
                self.merge_source_cursors = [
                    Some(SstVersionCursor::new(job.old0.handle)),
                    Some(SstVersionCursor::new(job.old1.handle)),
                ];
                job.phase = MergePhase::Schedule {
                    rank: 0,
                    resume_lo: SstKey::MIN,
                };
                job.schedule_len = 0;
                job.count = 0;
                job.crc = Crc32c::new();
                self.merge_job = Some(job);
                reset?;
                return Err(error);
            }
        };
        #[cfg(feature = "checkpoint-profile")]
        profile_checkpoint_phase(
            phase,
            storage.lsn(),
            Some(job.slot),
            started,
            before,
            self.blocks
                .lock()
                .expect("block store lock poisoned")
                .io_stats(),
            0,
        );
        match outcome {
            MergeBeatOutcome::Continue => self.merge_job = Some(job),
            MergeBeatOutcome::Cancel => {}
            MergeBeatOutcome::Finished(merged) => {
                self.merge_done[job.slot] = Some(CompletedMerge {
                    at: job.at,
                    old0: job.old0,
                    old1: job.old1,
                    merged,
                });
            }
        }
        Ok(())
    }

    /// The next pair worth merging: the first live table whose published
    /// list is at the trigger, taking its cheapest adjacent pair — least
    /// write amplification now, big settled members left to accrete —
    /// skipping pairs the id scratch cannot hold and pairs whose scans
    /// previously overflowed it. A dirty filled list is always considered,
    /// even below the ordinary merge trigger: publication needs one free slot
    /// and must not fall back to an unbounded full-table rewrite.
    fn merge_candidate(&self, storage: &Storage) -> Option<MergeJob> {
        if self.merge_job.is_some() {
            return None;
        }
        // A dirty full list cannot append its next delta. Free one of those
        // lists before servicing ordinary merge candidates, or a smaller
        // unrelated table can starve the publication.
        let must_free_full_list = (0..storage.physical_table_count()).any(|slot| {
            storage.table(slot).live()
                && storage.table(slot).definition().persistence
                    != crate::storage::RelationPersistence::Temporary
                && storage.table(slot).dirty()
                && self.merge_done[slot].is_none()
                && self
                    .prev_ssts
                    .get(slot)
                    .is_some_and(|list| list.n == list.capacity())
        });
        for slot in 0..storage.physical_table_count() {
            if !storage.table(slot).live()
                || storage.table(slot).definition().persistence
                    == crate::storage::RelationPersistence::Temporary
            {
                continue;
            }
            let Some(list) = self.prev_ssts.get(slot) else {
                continue;
            };
            if must_free_full_list && (!storage.table(slot).dirty() || list.n != list.capacity()) {
                continue;
            }
            if self.merge_done[slot].is_some() {
                continue;
            }
            if list.n < MERGE_TRIGGER && !must_free_full_list {
                continue;
            }
            let at = (0..list.n - 1)
                .min_by_key(|&i| {
                    list.ssts[i].expect("counted").count + list.ssts[i + 1].expect("counted").count
                })
                .expect("trigger implies at least one pair");
            let old0 = list.ssts[at].expect("counted");
            let old1 = list.ssts[at + 1].expect("counted");
            if (old0.count + old1.count) as usize > self.merge_scratch.capacity() {
                continue;
            }
            if self.merge_overflow.get(slot).copied().flatten()
                == Some((old0.handle.index, old1.handle.index))
            {
                continue;
            }
            return Some(MergeJob {
                slot,
                at,
                old0,
                old1,
                drop_tombstones: at == 0 && !storage.has_active_snapshots(),
                oldest_snapshot: storage.oldest_snapshot(),
                phase: MergePhase::Schedule {
                    rank: 0,
                    resume_lo: SstKey::MIN,
                },
                schedule_len: 0,
                count: 0,
                crc: Crc32c::new(),
            });
        }
        None
    }

    /// Whether compaction has anything to do: a job mid-flight, a finished
    /// merge awaiting its publish, or a published list at the trigger.
    pub(crate) fn merge_work_pending(&self, storage: &Storage) -> bool {
        self.merge_job.is_some()
            || self.merge_done.iter().any(Option::is_some)
            || self.merge_candidate(storage).is_some()
    }

    pub(crate) fn physical_maintenance_pending(&self, storage: &Storage) -> bool {
        self.published_lsn_pending_maintenance.is_some()
            || storage.retained_file_reclamation_pending()
    }

    pub(crate) fn maintenance_pending(&self, storage: &Storage) -> bool {
        self.physical_maintenance_pending(storage) || self.commit_cleanup_pending()
    }

    pub(crate) fn published_maintenance_pending(&self) -> bool {
        self.published_lsn_pending_maintenance.is_some()
    }

    pub(crate) fn commit_cleanup_pending(&self) -> bool {
        self.commit_prune_through.is_some()
    }

    pub(crate) fn metadata_io_ready(&self) -> bool {
        self.metadata_io.ready()
    }

    pub(crate) fn advance_commit_cleanup(&mut self) -> Result<bool, SqlError> {
        if let Some(floor) = self.commit_prune_through
            && self.prune_commit_batch(floor)?
        {
            self.commit_prune_through = self.commit_prune_next.take();
            return Ok(true);
        }
        Ok(false)
    }

    /// A schedule beat: scan a bounded stretch of one member, collecting
    /// `(rowid, commit_lsn, source-rank | tombstone-bit)`. When both members
    /// are done, exact duplicate keys choose the newer member, then the
    /// oldest-snapshot watermark prunes each row's physical chain.
    fn merge_schedule_beat(
        &mut self,
        job: &mut MergeJob,
        rank: u8,
        resume_lo: SstKey,
    ) -> Result<MergeBeatOutcome, SqlError> {
        self.sst_arena.reset();
        let mut reader = SstReader::new(&self.sst_arena).map_err(sst_to_sql)?;
        let member = if rank == 0 { &job.old0 } else { &job.old1 };
        let scratch = &mut self.merge_scratch;
        let blocks = &self.blocks;
        let mut overflow = false;
        let next = reader
            .scan_versions_bounded(
                &mut *blocks.lock().expect("block store lock poisoned"),
                &member.handle,
                resume_lo,
                MERGE_SCHEDULE_BEAT_BLOCKS,
                &mut |key, tombstone| {
                    if scratch.len() == scratch.capacity() {
                        overflow = true;
                        return;
                    }
                    scratch.push((key, rank | (u8::from(tombstone) << 1)));
                },
            )
            .map_err(sst_to_sql)?;
        if overflow {
            // The pair's counts under-reported its entries (corruption would
            // show elsewhere); remember it so the scheduler stops proposing
            // a merge that cannot be scheduled.
            if job.slot < self.merge_overflow.len() {
                self.merge_overflow[job.slot] =
                    Some((job.old0.handle.index, job.old1.handle.index));
            }
            return Ok(MergeBeatOutcome::Cancel);
        }
        job.phase = match (next, rank) {
            (Some(lo), _) => MergePhase::Schedule {
                rank,
                resume_lo: lo,
            },
            (None, 0) => MergePhase::Schedule {
                rank: 1,
                resume_lo: SstKey::MIN,
            },
            (None, _) => {
                // Exact duplicates choose the newer list member.
                self.merge_scratch
                    .sort_unstable_by_key(|&(key, kind)| (key, kind & 1));
                let mut keep = 0usize;
                for i in 0..self.merge_scratch.len() {
                    if keep > 0 && self.merge_scratch[keep - 1].0 == self.merge_scratch[i].0 {
                        self.merge_scratch[keep - 1] = self.merge_scratch[i];
                    } else {
                        self.merge_scratch[keep] = self.merge_scratch[i];
                        keep += 1;
                    }
                }
                // Keep every version newer than the oldest live snapshot and
                // one baseline version at/below it. With no live snapshot,
                // only the newest physical version of each row survives.
                let mut retained = 0usize;
                let mut at = 0usize;
                while at < keep {
                    let rowid = self.merge_scratch[at].0.rowid;
                    let mut kept_baseline = false;
                    while at < keep && self.merge_scratch[at].0.rowid == rowid {
                        let lsn = self.merge_scratch[at].0.commit_lsn;
                        let retain = match job.oldest_snapshot {
                            Some(oldest) => lsn > oldest || !kept_baseline,
                            None => {
                                retained == 0 || self.merge_scratch[retained - 1].0.rowid != rowid
                            }
                        };
                        if retain {
                            if job.oldest_snapshot.is_some_and(|oldest| lsn <= oldest) {
                                kept_baseline = true;
                            }
                            self.merge_scratch[retained] = self.merge_scratch[at];
                            retained += 1;
                        }
                        at += 1;
                    }
                }
                job.schedule_len = retained;
                MergePhase::Write { cursor: 0 }
            }
        };
        Ok(MergeBeatOutcome::Continue)
    }

    /// A write beat: stream scheduled entries into the merged SST until a
    /// few output blocks have been emitted (or a cheap-entry cap trips on a
    /// tombstone-heavy stretch), then suspend. Each source cursor and decoded
    /// group survives the suspension, so a rowid-ordered walk materializes an
    /// immutable source group once even when output pacing splits its rows.
    fn merge_write_beat(
        &mut self,
        job: &mut MergeJob,
        cursor: usize,
    ) -> Result<MergeBeatOutcome, SqlError> {
        let blocks = &self.blocks;
        let writer = &mut self.merge_writer;
        let output = &mut self.block_output;
        let scratch = &self.merge_scratch;
        let start_blocks = writer.roster_so_far().len();
        let start_reads = blocks
            .lock()
            .expect("block store lock poisoned")
            .io_stats()
            .object_gets;
        let mut cursor = cursor;
        let mut processed = 0usize;
        while cursor < job.schedule_len {
            if !output.is_empty()
                || processed >= MERGE_BEAT_ENTRIES
                || writer.roster_so_far().len() - start_blocks >= MERGE_WRITE_BEAT_BLOCKS
                || (processed > 0
                    && blocks
                        .lock()
                        .expect("block store lock poisoned")
                        .io_stats()
                        .object_gets
                        .saturating_sub(start_reads)
                        >= MERGE_WRITE_BEAT_READS)
            {
                job.phase = MergePhase::Write { cursor };
                return Ok(MergeBeatOutcome::Continue);
            }
            let (key, kind) = scratch[cursor];
            let rowid = key.rowid;
            let rank = usize::from(kind & 1);
            let decoded = if rank == 0 {
                &mut self.merge_source_decoded[..crate::store::MAX_PAYLOAD]
            } else {
                &mut self.merge_source_decoded[crate::store::MAX_PAYLOAD..]
            };
            let group = self.merge_source_cursors[rank]
                .as_mut()
                .expect("merge source cursor initialized with the job")
                .load_reusable_pax_group(
                    &mut *blocks.lock().expect("block store lock poisoned"),
                    key,
                    &mut self.merge_source_index,
                    &mut self.merge_source_raw,
                    decoded,
                    &mut self.merge_source_column,
                    &mut self.merge_row,
                )
                .map_err(sst_to_sql)?;
            if let Some(group) = group {
                let mut at = 0usize;
                let mut entries = 0usize;
                let mut reusable = true;
                while at < group.data_len() {
                    let Some((group_key, tombstone, _, next)) =
                        block_keys_at(&decoded[..group.data_len()], at)
                    else {
                        return Err(sql_err!(SQLSTATE_IO, "merge source group is truncated"));
                    };
                    let Some(&(scheduled_key, scheduled_kind)) = scratch.get(cursor + entries)
                    else {
                        reusable = false;
                        break;
                    };
                    if group_key != scheduled_key
                        || usize::from(scheduled_kind & 1) != rank
                        || tombstone != (scheduled_kind & 2 != 0)
                        || (tombstone && job.drop_tombstones)
                    {
                        reusable = false;
                        break;
                    }
                    entries += 1;
                    at = next;
                }
                if reusable && at == group.data_len() && entries > 0 {
                    let reused = writer
                        .append_reused_pax_group(
                            output,
                            group.reference(),
                            &self.merge_source_raw[..group.raw_len()],
                        )
                        .map_err(sst_to_sql)?;
                    if reused != entries {
                        return Err(sql_err!(
                            SQLSTATE_IO,
                            "merge source group changed while read"
                        ));
                    }
                    let mut at = 0usize;
                    for _ in 0..entries {
                        let (group_key, tombstone, len, next) =
                            block_keys_at(&decoded[..group.data_len()], at).ok_or_else(|| {
                                sql_err!(SQLSTATE_IO, "merge source group is truncated")
                            })?;
                        let mut header = [0u8; VERSIONED_SST_ENTRY_HEADER];
                        header[0..8].copy_from_slice(&group_key.rowid.to_le_bytes());
                        header[8..16].copy_from_slice(&group_key.commit_lsn.to_le_bytes());
                        if tombstone {
                            header[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
                        } else {
                            header[16..20].copy_from_slice(&len.to_le_bytes());
                        }
                        job.crc.update(&header);
                        if !tombstone {
                            let len = len as usize;
                            copy_block_entry_at(
                                &mut *blocks.lock().expect("block store lock poisoned"),
                                &decoded[..group.data_len()],
                                at,
                                &mut self.merge_row[..len],
                            )
                            .map_err(sst_to_sql)?;
                            job.crc.update(&self.merge_row[..len]);
                        }
                        at = next;
                    }
                    self.merge_source_cursors[rank]
                        .as_mut()
                        .expect("merge source cursor initialized with the job")
                        .consume_loaded_group();
                    cursor += entries;
                    processed += entries;
                    job.count += entries as u64;
                    continue;
                }
            }
            cursor += 1;
            processed += 1;
            if kind & 2 != 0 {
                // A tombstone: its within-pair row (if any) lost the dedup.
                // At the list head nothing older remains to suppress — drop
                // it; elsewhere it still shadows earlier members at cold
                // start, so it survives into the merged SST.
                if !job.drop_tombstones {
                    let mut header = [0u8; VERSIONED_SST_ENTRY_HEADER];
                    header[0..8].copy_from_slice(&rowid.to_le_bytes());
                    header[8..16].copy_from_slice(&key.commit_lsn.to_le_bytes());
                    header[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
                    job.crc.update(&header);
                    writer
                        .append_tombstone_version(output, key)
                        .map_err(sst_to_sql)?;
                    job.count += 1;
                }
                continue;
            }
            let len = self.merge_source_cursors[rank]
                .as_mut()
                .expect("merge source cursor initialized with the job")
                .copy_exact(
                    &mut *blocks.lock().expect("block store lock poisoned"),
                    key,
                    &mut self.merge_source_index,
                    &mut self.merge_source_raw,
                    decoded,
                    &mut self.merge_source_column,
                    &mut self.merge_row,
                )
                .map_err(sst_to_sql)?
                .filter(|probe| probe.key == key && probe.len.is_some())
                .ok_or_else(|| {
                    sql_err!(
                        SQLSTATE_IO,
                        "merge lost row {} between scan and read",
                        rowid
                    )
                })?;
            let len = len.len.expect("filtered live version") as usize;
            let mut header = [0u8; VERSIONED_SST_ENTRY_HEADER];
            header[0..8].copy_from_slice(&rowid.to_le_bytes());
            header[8..16].copy_from_slice(&key.commit_lsn.to_le_bytes());
            header[16..20].copy_from_slice(&(len as u32).to_le_bytes());
            job.crc.update(&header);
            job.crc.update(&self.merge_row[..len]);
            writer
                .append_version(output, key, &self.merge_row[..len])
                .map_err(sst_to_sql)?;
            job.count += 1;
        }
        if !output.is_empty() {
            job.phase = MergePhase::Write { cursor };
            return Ok(MergeBeatOutcome::Continue);
        }
        if job.count == 0 {
            return Ok(MergeBeatOutcome::Finished(None));
        }
        let handle = writer
            .finish(output)
            .map_err(sst_to_sql)?
            .ok_or_else(|| sql_err!(SQLSTATE_IO, "merge wrote rows but produced no SST"))?;
        job.phase = MergePhase::Finish {
            merged: Some(PrevSst {
                handle,
                count: job.count,
                crc: job.crc.finish(),
            }),
        };
        Ok(MergeBeatOutcome::Continue)
    }

    /// Builds the clients and fixed cache/checkpoint buffers at startup.
    /// Provider credentials and adapter selection terminate at
    /// [`crate::object_store`].
    pub(crate) fn new(config: &Config, budget: &mut Budget) -> Result<Self, CheckpointSetupError> {
        if config.checkpoint_delete_objects_per_beat > config.checkpoint_garbage_batch_objects {
            return Err(CheckpointSetupError::ObjectStore(
                "checkpoint_delete_objects_per_beat must not exceed checkpoint_garbage_batch_objects"
                    .to_string(),
            ));
        }
        let table_capacity = crate::storage::table_slot_capacity(config);
        let manifest_capacity = config.checkpoint_manifest_bytes;
        budget
            .draw(
                core::mem::size_of::<Option<ManifestPublication>>(),
                "manifest publication state",
            )
            .map_err(CheckpointSetupError::Budget)?;
        budget
            .draw(
                Self::workspace_budget_bytes(config),
                "checkpoint workspaces",
            )
            .map_err(CheckpointSetupError::Budget)?;
        let base = OwnedObjectStore::new(config, budget, "blocks/")
            .map_err(|error| CheckpointSetupError::ObjectStore(error.to_string()))?;
        let plan = StackPlan::resolve(config.block_cache_bytes, config.disk_cache_bytes);
        if plan.undersized_ram() || plan.undersized_disk() {
            return Err(CheckpointSetupError::ObjectStore(
                "block_cache_bytes / disk_cache_bytes smaller than one block; set 0 to disable a tier"
                    .to_string(),
            ));
        }
        // The WAL creates the data directory later in startup; the disk
        // cache's slot file needs it now.
        std::fs::create_dir_all(&config.data_dir)
            .map_err(|e| CheckpointSetupError::ObjectStore(format!("create data_dir: {e}")))?;
        let cache_dir = std::path::Path::new(&config.data_dir);
        let blocks = std::sync::Arc::new(Mutex::new(
            crate::store::build_tiers(budget, base, plan, cache_dir).map_err(|e| {
                CheckpointSetupError::ObjectStore(format!("block cache stack: {e:?}"))
            })?,
        ));
        budget
            .draw_array(
                2usize
                    .saturating_mul(table_capacity)
                    .saturating_add(1)
                    .saturating_mul(config.max_spill_generations_per_table),
                core::mem::size_of::<Option<PrevSst>>(),
                "checkpoint spill-generation rosters",
            )
            .map_err(CheckpointSetupError::Budget)?;
        let prev_ssts = (0..table_capacity)
            .map(|_| SlotList::new(config.max_spill_generations_per_table))
            .collect::<Vec<_>>();
        let prev_scratch = (0..table_capacity)
            .map(|_| SlotList::new(config.max_spill_generations_per_table))
            .collect::<Vec<_>>();
        budget
            .draw(
                ExternalRunReader::budget_bytes(),
                "checkpoint value-index run reader",
            )
            .map_err(CheckpointSetupError::Budget)?;
        budget
            .draw(
                ValueIndexStream::budget_bytes(config.checkpoint_live_blocks)
                    + crate::store::MAX_PAYLOAD,
                "checkpoint published value-index stream",
            )
            .map_err(CheckpointSetupError::Budget)?;
        budget
            .draw(
                2 * crate::store::MAX_PAYLOAD
                    + VALUE_SORT_ROWS_PER_CHUNK * core::mem::size_of::<BufferedValueRow>()
                    + CheckpointValueCursor::budget_bytes(
                        config.max_spill_generations_per_table,
                        config.table_rows,
                    )
                    + config.table_rows * core::mem::size_of::<u64>(),
                "checkpoint value-index scheduling",
            )
            .map_err(CheckpointSetupError::Budget)?;
        budget
            .draw(
                MERGE_SOURCE_SCRATCH_BYTES,
                "checkpoint row-merge source cursors",
            )
            .map_err(CheckpointSetupError::Budget)?;
        budget
            .draw(
                garbage_sst_root_capacity(config) * core::mem::size_of::<SstHandle>(),
                "garbage collection retained SST roots",
            )
            .map_err(CheckpointSetupError::Budget)?;
        budget
            .draw(
                garbage_index_root_capacity(config) * core::mem::size_of::<BlockId>(),
                "garbage collection retained index roots",
            )
            .map_err(CheckpointSetupError::Budget)?;
        let writer_token = new_writer_token().map_err(|error| {
            CheckpointSetupError::ObjectStore(format!("create writer token: {error}"))
        })?;
        Ok(Self {
            client: ObjectStore::new(config, budget)
                .map_err(|error| CheckpointSetupError::ObjectStore(error.to_string()))?,
            metadata_io: MetadataIo::new(config, budget)?,
            uploads: crate::object_store::upload::Uploads::new(
                config,
                config.object_store_put_slots,
                config.wal_upload_buffer_bytes,
                budget,
            )
            .map_err(|error| CheckpointSetupError::ObjectStore(error.to_string()))?,
            block_uploads: crate::object_store::upload::Uploads::new(
                config,
                config.object_store_block_put_slots,
                crate::store::BLOCK_SIZE,
                budget,
            )
            .map_err(|error| CheckpointSetupError::ObjectStore(error.to_string()))?,
            manifest_uploads: crate::object_store::upload::Uploads::new(
                config,
                MANIFEST_UPLOAD_SLOTS,
                config.checkpoint_manifest_bytes,
                budget,
            )
            .map_err(|error| CheckpointSetupError::ObjectStore(error.to_string()))?,
            writer_probe: if config.operations_listen_addr.is_empty() {
                None
            } else {
                Some(WriterProbe::new(config, budget, writer_token)?)
            },
            credential_uploads: if config.object_store_credentials_file.is_empty() {
                None
            } else {
                Some(
                    crate::object_store::upload::Uploads::new(
                        config,
                        CREDENTIAL_UPLOAD_SLOTS,
                        CREDENTIAL_UPLOAD_BYTES,
                        budget,
                    )
                    .map_err(|error| CheckpointSetupError::ObjectStore(error.to_string()))?,
                )
            },
            credential_renewal: None,
            manifest_publication: None,
            requested_checkpoint_epoch: 0,
            captured_checkpoint_epoch: 0,
            published_checkpoint_epoch: 0,
            block_output: crate::store::BlockWriteBatch::new(
                budget,
                PHYSICAL_EMISSION_BYTES,
                PHYSICAL_EMISSION_OBJECTS,
            )
            .map_err(CheckpointSetupError::Budget)?,
            table_output: FixedVec::new(
                budget,
                "checkpoint write entries",
                config.checkpoint_write_entries,
            )
            .map_err(CheckpointSetupError::Budget)?,
            index_output_owner: None,
            table_job: None,
            commit_publication: None,
            blocks,
            sst_arena: Arena::new(budget, "checkpoint sst", SST_ARENA_BYTES)
                .map_err(CheckpointSetupError::Budget)?,
            parameter_acl_arena: Arena::new(
                budget,
                "checkpoint parameter ACL row",
                Storage::parameter_acl_image_budget_bytes(config),
            )
            .map_err(CheckpointSetupError::Budget)?,
            pending_installs: Vec::with_capacity(
                table_capacity.saturating_mul(config.max_spill_generations_per_table),
            ),
            pending_value_installs: Vec::with_capacity(
                table_capacity * crate::storage::MAX_VALUE_ENFORCERS,
            ),
            merge_scratch: Vec::with_capacity(config.checkpoint_merge_entries),
            roster_scratch: Vec::with_capacity(config.checkpoint_live_blocks),
            garbage_seen: FixedMap::new(
                budget,
                "garbage collection block membership",
                config.checkpoint_live_blocks,
            )
            .map_err(CheckpointSetupError::Budget)?,
            garbage_sst_roots: Vec::with_capacity(garbage_sst_root_capacity(config)),
            garbage_index_roots: Vec::with_capacity(garbage_index_root_capacity(config)),
            doomed_blocks: Vec::with_capacity(config.checkpoint_garbage_batch_objects),
            manifest_buf: FixedBuf::new(budget, "manifest_buf", manifest_capacity)
                .map_err(CheckpointSetupError::Budget)?,
            manifest_output: FixedBuf::new(budget, "retained manifest", manifest_capacity)
                .map_err(CheckpointSetupError::Budget)?,
            prepared_transaction_snapshot: FixedVec::new(
                budget,
                "checkpoint prepared transaction snapshot",
                config.max_prepared_transactions,
            )
            .map_err(CheckpointSetupError::Budget)?,
            manifest_etag: None,
            manifest_present: false,
            manifest_lsn: 0,
            commit_head_etag: None,
            commit_head: None,
            root_buf: FixedBuf::new(budget, "mutable_root_buf", 128)
                .map_err(CheckpointSetupError::Budget)?,
            prev_ssts,
            prev_scratch,
            slice_scratch: SlotList::new(config.max_spill_generations_per_table),
            commit_scratch: Vec::with_capacity(config.checkpoint_commit_batches),
            backup_scratch: Vec::with_capacity(config.max_backups),
            backup_roster_loaded: false,
            garbage_scratch: Vec::with_capacity(config.checkpoint_garbage_batch_objects),
            prune_backup_scratch: Vec::with_capacity(config.max_backups),
            prune_backup_roster_loaded: false,
            commit_garbage_scratch: Vec::with_capacity(config.checkpoint_garbage_batch_objects),
            delete_objects_per_beat: config.checkpoint_delete_objects_per_beat,
            commit_prune: CommitPruneProgress::Unloaded,
            legacy_garbage_loaded: false,
            legacy_garbage_overflow: false,
            block_garbage_loaded: false,
            block_garbage_overflow: false,
            sweeping: false,
            published_lsn_pending_maintenance: None,
            commit_prune_through: None,
            commit_prune_next: None,
            legacy_garbage_pending: false,
            block_garbage_pending: false,
            sliced_generation: vec![0; table_capacity],
            sliced_lsn: vec![0; table_capacity],
            sliced_layout: vec![SliceLayout::EMPTY; table_capacity],
            sliced_this_sweep: vec![false; table_capacity],
            checkpoint_deletions: (0..table_capacity).map(|_| None).collect(),
            checkpoint_statistics: (0..table_capacity).map(|_| None).collect(),
            slice_writer: SstWriter::new(),
            merge_writer: SstWriter::new(),
            merge_source_cursors: [None, None],
            merge_source_decoded: vec![0; 2 * crate::store::MAX_PAYLOAD].into_boxed_slice(),
            merge_source_index: vec![0; crate::store::MAX_PAYLOAD].into_boxed_slice(),
            merge_source_raw: vec![0; crate::store::MAX_PAYLOAD].into_boxed_slice(),
            merge_source_column: vec![0; crate::store::MAX_PAYLOAD].into_boxed_slice(),
            merge_row: vec![0; MAX_ASSEMBLED].into_boxed_slice(),
            value_writer: ValueIndexWriter::new(),
            value_source: vec![0; crate::store::MAX_PAYLOAD].into_boxed_slice(),
            value_sort_rows: vec![EMPTY_BUFFERED_VALUE_ROW; VALUE_SORT_ROWS_PER_CHUNK]
                .into_boxed_slice(),
            value_source_cursor: CheckpointValueCursor::new(
                config.max_spill_generations_per_table,
                config.table_rows,
            ),
            value_sort_reader: ExternalRunReader::new(),
            value_changed_rowids: Vec::with_capacity(config.table_rows),
            value_base_stream: ValueIndexStream::new(config.checkpoint_live_blocks),
            value_base_entry: vec![0; crate::store::MAX_PAYLOAD].into_boxed_slice(),
            value_entry: vec![0; crate::store::MAX_PAYLOAD].into_boxed_slice(),
            value_schedule_job: None,
            value_job: None,
            merge_job: None,
            merge_done: vec![None; table_capacity],
            merge_turn: false,
            merge_overflow: vec![None; table_capacity],
            writer_token,
            writer_fence_etag: None,
            writer_fenced: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn root_etags_for_test(&self) -> (EntityTag, EntityTag) {
        (
            self.manifest_etag.expect("writer loaded a manifest root"),
            self.commit_head_etag
                .expect("writer loaded a commit-head root"),
        )
    }

    /// The shared block stack, for the storage layer's spilled-row reader.
    pub(crate) fn block_stack(&self) -> std::sync::Arc<Mutex<TieredStore<OwnedObjectStore>>> {
        std::sync::Arc::clone(&self.blocks)
    }

    #[cfg(test)]
    pub(crate) fn published_spill_generation_count(&self, slot: usize) -> usize {
        self.prev_ssts.get(slot).map_or(0, |list| list.n)
    }

    #[cfg(test)]
    pub(crate) fn sliced_generation(&self, slot: usize) -> u64 {
        self.sliced_generation.get(slot).copied().unwrap_or(0)
    }

    #[cfg(test)]
    pub(crate) fn merge_source_read_ready(&self) -> bool {
        self.block_output.is_empty()
            && self
                .merge_job
                .as_ref()
                .is_some_and(|job| matches!(job.phase, MergePhase::Write { .. }))
    }

    #[cfg(test)]
    pub(crate) fn value_index_job_active(&self) -> bool {
        self.value_schedule_job.is_some() || self.value_job.is_some()
    }

    #[cfg(test)]
    pub(crate) fn value_index_schedule_merge_ready(&self) -> bool {
        self.value_schedule_job
            .as_ref()
            .is_some_and(|job| matches!(job.phase, ValueSchedulePhase::StartMerge { .. }))
    }

    #[cfg(test)]
    pub(crate) fn value_index_output_job_active(&self) -> bool {
        self.value_job.is_some()
    }

    #[cfg(test)]
    pub(crate) fn pending_value_index_handle(
        &self,
        slot: usize,
        index_created_at: u64,
    ) -> Option<ValueIndexHandle> {
        self.pending_value_installs
            .iter()
            .find(|install| {
                install.slot == slot && install.index_created_at == Some(index_created_at)
            })
            .and_then(|install| install.handle)
    }

    pub(crate) fn atomic_block_read_scope(&self) -> AtomicBlockReadScope {
        let scope = self.deferred_atomic_block_read_scope();
        scope.activate();
        scope
    }

    pub(crate) fn deferred_atomic_block_read_scope(&self) -> AtomicBlockReadScope {
        AtomicBlockReadScope {
            blocks: std::sync::Arc::clone(&self.blocks),
            active: core::cell::Cell::new(false),
        }
    }

    pub(crate) fn enable_async_block_reads(&mut self) {
        self.blocks
            .lock()
            .expect("block store lock poisoned")
            .enable_async_gets();
    }

    #[cfg(test)]
    pub(crate) fn disable_async_block_reads(&mut self) {
        self.blocks
            .lock()
            .expect("block store lock poisoned")
            .disable_async_gets();
    }

    pub(crate) fn set_read_owner(&mut self, owner: Option<crate::store::ReadOwner>) {
        self.blocks
            .lock()
            .expect("block store lock poisoned")
            .set_read_owner(owner);
    }

    pub(crate) fn cancel_read_owner(&mut self, owner: crate::store::ReadOwner) {
        self.blocks
            .lock()
            .expect("block store lock poisoned")
            .cancel_read_owner(owner);
    }

    pub(crate) fn read_capacity_generation(&self) -> u64 {
        self.blocks
            .lock()
            .expect("block store lock poisoned")
            .read_capacity_generation()
    }

    pub(crate) fn block_read_slots(&self) -> usize {
        self.blocks
            .lock()
            .expect("block store lock poisoned")
            .async_read_slots()
    }

    pub(crate) fn block_reads_busy(&self) -> bool {
        self.blocks
            .lock()
            .expect("block store lock poisoned")
            .async_reads_busy()
    }

    pub(crate) fn object_io_busy(&self) -> bool {
        self.block_reads_busy()
            || self.metadata_io.busy()
            || self.uploads.busy()
            || self.block_uploads.busy()
            || self.manifest_uploads.busy()
            || self.writer_probe.as_ref().is_some_and(WriterProbe::busy)
            || self
                .credential_uploads
                .as_ref()
                .is_some_and(|uploads| uploads.busy())
            || self.commit_publication.is_some()
    }

    pub(crate) fn pending_block_read_fd(&self, slot: usize) -> Option<std::os::fd::RawFd> {
        self.blocks
            .lock()
            .expect("block store lock poisoned")
            .pending_read_fd(slot)
    }

    pub(crate) fn pending_block_read_wants_write(&self, slot: usize) -> bool {
        self.blocks
            .lock()
            .expect("block store lock poisoned")
            .pending_read_wants_write(slot)
    }

    pub(crate) fn advance_pending_block_read(
        &mut self,
        slot: usize,
    ) -> Result<bool, crate::store::StoreError> {
        self.blocks
            .lock()
            .expect("block store lock poisoned")
            .advance_pending_read(slot)
    }

    pub(crate) fn next_block_read_deadline(&self) -> Option<std::time::Instant> {
        self.blocks
            .lock()
            .expect("block store lock poisoned")
            .next_read_deadline()
    }

    pub(crate) fn advance_block_read_deadlines(&mut self, now: std::time::Instant) -> bool {
        self.blocks
            .lock()
            .expect("block store lock poisoned")
            .advance_read_deadlines(now)
    }

    fn claim_writer(&mut self) -> Result<(), CheckpointSetupError> {
        let promoted = promote_writer(
            &mut self.client,
            self.writer_token,
            &mut self.manifest_buf,
            &mut self.root_buf,
            PromotionRoots::Validated {
                manifest: self.manifest_etag,
                head: self.commit_head_etag,
            },
        )?;
        self.writer_fence_etag = Some(promoted.fence.etag);
        self.manifest_etag = Some(promoted.manifest);
        self.commit_head_etag = Some(promoted.head);
        Ok(())
    }

    pub(crate) fn writer_was_fenced(&self) -> bool {
        self.writer_fenced || self.writer_probe.as_ref().is_some_and(WriterProbe::fenced)
    }

    pub(crate) fn writer_probe(&mut self) -> Option<&mut WriterProbe> {
        if self.writer_fenced
            && let Some(probe) = self.writer_probe.as_mut()
        {
            probe.fence();
        }
        self.writer_probe.as_mut()
    }

    #[cfg(test)]
    pub(crate) fn verify_writer(&mut self) -> Result<(), SqlError> {
        if self.writer_was_fenced() {
            return Err(sql_err!(
                SQLSTATE_CAS,
                "writer was fenced by a newer process"
            ));
        }
        let active = writer_fence_record("active", self.writer_token);
        self.client
            .get(WRITER_FENCE_KEY, None)
            .map_err(|error| sql_err!(SQLSTATE_CAS, "verify writer ownership: {}", error))?;
        if self.client.body_bytes() != active.as_str().as_bytes() {
            self.writer_fenced = true;
            return Err(sql_err!(
                SQLSTATE_CAS,
                "writer was fenced by a newer process"
            ));
        }
        Ok(())
    }

    pub(crate) fn credential_install_pending(&self) -> bool {
        matches!(
            self.credential_renewal
                .as_ref()
                .map(|renewal| &renewal.phase),
            Some(CredentialRenewalPhase::Installing(_))
        )
    }

    fn set_credential_admission(&mut self, admission: crate::object_store::RequestAdmission) {
        // Reads can be repeated; DELETE is idempotent and its position advances
        // only on consumption. Releasing this request cannot publish new data.
        if admission == crate::object_store::RequestAdmission::Draining {
            self.metadata_io.cancel();
        }
        self.metadata_io.set_admission(admission);
        self.block_uploads.set_admission(admission);
        self.blocks
            .lock()
            .expect("block store lock poisoned")
            .base_mut()
            .set_admission(admission);
        if let Some(probe) = self.writer_probe.as_mut() {
            probe.set_admission(admission);
        }
    }

    pub(crate) fn begin_credential_reload(
        &mut self,
        credentials: crate::object_store::Credentials,
    ) -> Result<(), SqlError> {
        if self.credential_renewal.is_some() {
            return Err(sql_err!(
                sqlstate::INTERNAL_IO_WAIT,
                "credential renewal already pending"
            ));
        }
        if self.writer_was_fenced() {
            return Err(sql_err!(
                SQLSTATE_CAS,
                "writer was fenced by a newer process"
            ));
        }
        if self.client.credentials().is_none() {
            return Err(sql_err!(
                SQLSTATE_IO,
                "object-store credential rotation is unavailable for the simulator"
            ));
        }
        let etag = self.writer_fence_etag.ok_or_else(|| {
            sql_err!(
                SQLSTATE_CAS,
                "writer ownership was not acquired during startup"
            )
        })?;
        let uploads = self.credential_uploads.as_mut().ok_or_else(|| {
            sql_err!(
                SQLSTATE_IO,
                "credential renewal requires object_store_credentials_file"
            )
        })?;
        uploads.replace_credentials(credentials);
        let record = writer_fence_record("active", self.writer_token);
        let ticket = uploads
            .enqueue(
                WRITER_FENCE_KEY,
                record.as_str().as_bytes(),
                crate::object_store::upload::Condition::PublishMatchVerified(etag),
            )
            .map_err(upload_to_sql)?;
        self.credential_renewal = Some(CredentialRenewal {
            credentials,
            phase: CredentialRenewalPhase::Uploading(ticket),
        });
        Ok(())
    }

    pub(crate) fn advance_credential_reload(&mut self) -> Result<CredentialReloadStep, SqlError> {
        let mut renewal = self.credential_renewal.take().ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "credential renewal has not been requested"
            )
        })?;
        if self.writer_was_fenced() {
            self.set_credential_admission(crate::object_store::RequestAdmission::Open);
            if let CredentialRenewalPhase::Uploading(ticket) = renewal.phase {
                self.credential_uploads
                    .as_mut()
                    .expect("reserved credential client")
                    .cancel(ticket)
                    .map_err(upload_to_sql)?;
            }
            return Err(sql_err!(
                SQLSTATE_CAS,
                "writer was fenced by a newer process"
            ));
        }
        match renewal.phase {
            CredentialRenewalPhase::Uploading(ticket) => {
                match self
                    .credential_uploads
                    .as_mut()
                    .expect("reserved credential client")
                    .take(ticket)
                {
                    Ok(Some(etag)) => {
                        renewal.phase = CredentialRenewalPhase::Installing(etag);
                        self.set_credential_admission(
                            crate::object_store::RequestAdmission::Draining,
                        );
                    }
                    Ok(None) => {}
                    Err(error) => {
                        if matches!(&error, crate::object_store::upload::UploadError::Request(error)
                            if error.is_precondition_failed())
                        {
                            self.writer_fenced = true;
                        }
                        return Err(upload_to_sql(error));
                    }
                }
                self.credential_renewal = Some(renewal);
                Ok(CredentialReloadStep::Pending)
            }
            CredentialRenewalPhase::Installing(etag) => {
                // Every transport retains its signed request until completion.
                // Install validated credentials only between those owners.
                if self.object_io_busy() {
                    self.credential_renewal = Some(renewal);
                    return Ok(CredentialReloadStep::Pending);
                }
                self.client.replace_credentials(renewal.credentials);
                self.metadata_io.replace_credentials(renewal.credentials);
                self.uploads.replace_credentials(renewal.credentials);
                self.block_uploads.replace_credentials(renewal.credentials);
                self.manifest_uploads
                    .replace_credentials(renewal.credentials);
                if let Some(probe) = self.writer_probe.as_mut() {
                    probe.replace_credentials(renewal.credentials);
                }
                self.blocks
                    .lock()
                    .expect("block store lock poisoned")
                    .base_mut()
                    .replace_credentials(renewal.credentials);
                self.writer_fence_etag = Some(etag);
                self.set_credential_admission(crate::object_store::RequestAdmission::Open);
                Ok(CredentialReloadStep::Installed)
            }
        }
    }

    /// Publishes one immutable committed journal batch, keyed by its first
    /// LSN.  The local journal is a cache; cold recovery obtains this tail
    /// from object storage after loading the manifest.
    pub(crate) fn publish_commit_batch(
        &mut self,
        first_lsn: u64,
        bytes: &[u8],
    ) -> Result<(), SqlError> {
        self.begin_commit_publication(first_lsn, bytes)?;
        while !self.advance_commit_publication(std::time::Instant::now())? {
            if let Err(error) = self.uploads.wait_ready() {
                self.cancel_commit_publication()?;
                return Err(object_store_to_sql(error));
            }
        }
        Ok(())
    }

    pub(crate) fn begin_commit_publication(
        &mut self,
        first_lsn: u64,
        bytes: &[u8],
    ) -> Result<(), SqlError> {
        if self.commit_publication.is_some() {
            return Err(sql_err!(
                sqlstate::INTERNAL_IO_WAIT,
                "commit publication already in progress"
            ));
        }
        let batch = CommitBatchId::from_bytes(first_lsn, bytes);
        let key = stack_format!(
            72,
            "commits/{:020}-{:08x}.batch",
            batch.first_lsn,
            batch.digest
        );
        let descriptor_key = stack_format!(
            72,
            "commits/{:020}-{:08x}.head",
            batch.first_lsn,
            batch.digest
        );
        let previous = self.commit_head.unwrap_or(CommitBatchId::EMPTY);
        let mut descriptor = StackStr::<96>::new();
        use core::fmt::Write;
        write!(
            descriptor,
            "{}\nfirst {}\ndigest {:08x}\nprevious {} {:08x}\nend\n",
            COMMIT_HEAD_HEADER, batch.first_lsn, batch.digest, previous.first_lsn, previous.digest
        )
        .expect("commit descriptor fits its fixed buffer");
        use crate::object_store::upload::Condition;
        let data_ticket = self
            .uploads
            .enqueue(key.as_str(), bytes, Condition::ImmutableCreate)
            .map_err(upload_to_sql)?;
        let descriptor_ticket = match self.uploads.enqueue(
            descriptor_key.as_str(),
            descriptor.as_str().as_bytes(),
            Condition::ImmutableCreate,
        ) {
            Ok(ticket) => ticket,
            Err(error) => {
                self.uploads.cancel(data_ticket).map_err(upload_to_sql)?;
                return Err(upload_to_sql(error));
            }
        };
        self.commit_publication = Some(CommitPublication::Objects {
            batch,
            data: Some(data_ticket),
            descriptor: Some(descriptor_ticket),
        });
        Ok(())
    }

    /// Independent objects may complete out of order. Only the sequenced root
    /// transition makes their transaction records recoverable.
    pub(crate) fn advance_commit_publication(
        &mut self,
        now: std::time::Instant,
    ) -> Result<bool, SqlError> {
        use crate::object_store::upload::Condition;
        self.uploads.advance(now);
        let Some(publication) = self.commit_publication.take() else {
            return Ok(true);
        };
        match publication {
            CommitPublication::Objects {
                batch,
                mut data,
                mut descriptor,
            } => {
                if let Some(ticket) = data {
                    match self.uploads.take(ticket) {
                        Ok(Some(_)) => data = None,
                        Ok(None) => {}
                        Err(error) => {
                            if let Some(ticket) = descriptor {
                                self.uploads.cancel(ticket).map_err(upload_to_sql)?;
                            }
                            return Err(upload_to_sql(error));
                        }
                    }
                }
                if let Some(ticket) = descriptor {
                    match self.uploads.take(ticket) {
                        Ok(Some(_)) => descriptor = None,
                        Ok(None) => {}
                        Err(error) => {
                            if let Some(ticket) = data {
                                self.uploads.cancel(ticket).map_err(upload_to_sql)?;
                            }
                            return Err(upload_to_sql(error));
                        }
                    }
                }
                if data.is_none() && descriptor.is_none() {
                    let head = format_commit_head(self.writer_token, batch);
                    let condition = match self.commit_head_etag {
                        Some(etag) => Condition::PublishMatch(etag),
                        None => Condition::PublishAbsent,
                    };
                    let ticket = self
                        .uploads
                        .enqueue(COMMIT_HEAD_KEY, head.as_str().as_bytes(), condition)
                        .map_err(upload_to_sql)?;
                    self.commit_publication = Some(CommitPublication::Root { batch, ticket });
                } else {
                    self.commit_publication = Some(CommitPublication::Objects {
                        batch,
                        data,
                        descriptor,
                    });
                }
                Ok(false)
            }
            CommitPublication::Root { batch, ticket } => {
                let outcome = self.uploads.take(ticket).map_err(|error| match error {
                    crate::object_store::upload::UploadError::Request(error)
                        if error.is_precondition_failed() =>
                    {
                        sql_err!(SQLSTATE_CAS, "commit root changed; writer was fenced")
                    }
                    other => upload_to_sql(other),
                })?;
                match outcome {
                    Some(etag) => {
                        self.commit_head_etag = Some(etag);
                        self.commit_head = Some(batch);
                        Ok(true)
                    }
                    None => {
                        self.commit_publication = Some(CommitPublication::Root { batch, ticket });
                        Ok(false)
                    }
                }
            }
        }
    }

    pub(crate) fn wait_commit_publication(&mut self) -> Result<(), SqlError> {
        if self.commit_publication.is_none() {
            return Ok(());
        }
        self.uploads.wait_ready().map_err(object_store_to_sql)
    }

    fn cancel_commit_publication(&mut self) -> Result<(), SqlError> {
        match self.commit_publication.take() {
            Some(CommitPublication::Objects {
                data, descriptor, ..
            }) => {
                for ticket in [data, descriptor].into_iter().flatten() {
                    self.uploads.cancel(ticket).map_err(upload_to_sql)?;
                }
            }
            Some(CommitPublication::Root { ticket, .. }) => {
                self.uploads.cancel(ticket).map_err(upload_to_sql)?;
            }
            None => {}
        }
        Ok(())
    }

    pub(crate) fn commit_publication_busy(&self) -> bool {
        self.commit_publication.is_some()
    }

    pub(crate) fn background_io_slots(&self) -> usize {
        1 + self.uploads.slots()
            + self.block_uploads.slots()
            + self.manifest_uploads.slots()
            + self
                .credential_uploads
                .as_ref()
                .map_or(0, |uploads| uploads.slots())
    }

    pub(crate) fn pending_background_io_fd(&self, slot: usize) -> Option<std::os::fd::RawFd> {
        if slot == self.background_io_slots() - 1 {
            return self.metadata_io.fd();
        }
        if slot < self.uploads.slots() {
            self.uploads.fd(slot)
        } else if slot < self.uploads.slots() + self.block_uploads.slots() {
            self.block_uploads.fd(slot - self.uploads.slots())
        } else if slot
            < self.uploads.slots() + self.block_uploads.slots() + self.manifest_uploads.slots()
        {
            self.manifest_uploads
                .fd(slot - self.uploads.slots() - self.block_uploads.slots())
        } else {
            self.credential_uploads
                .as_ref()
                .expect("reserved credential upload slot")
                .fd(slot
                    - self.uploads.slots()
                    - self.block_uploads.slots()
                    - self.manifest_uploads.slots())
        }
    }

    pub(crate) fn pending_background_io_wants_write(&self, slot: usize) -> bool {
        if slot == self.background_io_slots() - 1 {
            return self.metadata_io.wants_write();
        }
        if slot < self.uploads.slots() {
            self.uploads.wants_write(slot)
        } else if slot < self.uploads.slots() + self.block_uploads.slots() {
            self.block_uploads.wants_write(slot - self.uploads.slots())
        } else if slot
            < self.uploads.slots() + self.block_uploads.slots() + self.manifest_uploads.slots()
        {
            self.manifest_uploads
                .wants_write(slot - self.uploads.slots() - self.block_uploads.slots())
        } else {
            self.credential_uploads
                .as_ref()
                .expect("reserved credential upload slot")
                .wants_write(
                    slot - self.uploads.slots()
                        - self.block_uploads.slots()
                        - self.manifest_uploads.slots(),
                )
        }
    }

    pub(crate) fn next_background_io_deadline(&self) -> Option<std::time::Instant> {
        [
            self.metadata_io.deadline(),
            self.uploads.next_deadline(),
            self.block_output.deadline(&self.block_uploads),
            self.manifest_uploads.next_deadline(),
            self.credential_uploads
                .as_ref()
                .and_then(|uploads| uploads.next_deadline()),
            match self
                .credential_renewal
                .as_ref()
                .map(|renewal| &renewal.phase)
            {
                Some(CredentialRenewalPhase::Installing(_)) if !self.object_io_busy() => {
                    Some(std::time::Instant::now())
                }
                _ => None,
            },
            match self.manifest_publication {
                Some(ManifestPublication::Retry { at, .. }) => Some(at),
                Some(ManifestPublication::Installing { .. }) => Some(std::time::Instant::now()),
                _ => None,
            },
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub(crate) fn advance_background_io(&mut self, now: std::time::Instant) {
        self.metadata_io.advance(now);
        self.block_uploads.advance(now);
        self.manifest_uploads.advance(now);
        if let Some(uploads) = self.credential_uploads.as_mut() {
            uploads.advance(now);
        }
    }

    pub(crate) fn physical_output_ready(&self, now: std::time::Instant) -> bool {
        self.block_output.ready(&self.block_uploads, now)
    }

    pub(crate) fn physical_output_pending(&self) -> bool {
        !self.block_output.is_empty()
    }

    pub(crate) fn physical_output_stale(&self, storage: &Storage) -> bool {
        !self.block_output.is_empty()
            && match self.index_output_owner {
                Some(IndexOutputOwner::Schedule) => self
                    .value_schedule_job
                    .as_ref()
                    .is_none_or(|job| !Self::value_schedule_job_is_current(storage, job)),
                Some(IndexOutputOwner::Index) => self
                    .value_job
                    .as_ref()
                    .is_none_or(|job| !Self::value_job_is_current(storage, job)),
                None => false,
            }
    }

    pub(crate) fn advance_physical_output(&mut self, storage: &Storage) -> Result<bool, SqlError> {
        if self.physical_output_stale(storage) {
            self.block_output
                .discard(&mut self.block_uploads)
                .map_err(upload_to_sql)?;
            self.index_output_owner = None;
            return Ok(true);
        }
        self.drain_block_output()
    }

    pub(crate) fn retained_output_pending(&self) -> bool {
        self.table_job.is_some() || self.merge_job.is_some()
    }

    pub(crate) fn retained_output_ready(&self) -> bool {
        self.block_output.is_empty() && self.retained_output_pending()
    }

    /// Already-captured inputs can progress behind later journal uploads.
    /// This path never starts a sweep or publishes a manifest.
    pub(crate) fn advance_retained_output(&mut self, storage: &Storage) -> Result<(), SqlError> {
        assert!(
            self.retained_output_ready(),
            "retained producer must wait for its previous emission"
        );
        if self.table_job.is_some() && !(self.merge_turn && self.merge_job.is_some()) {
            self.merge_turn = true;
            self.advance_table_slice(storage)
        } else {
            self.merge_turn = false;
            self.merge_beat(storage)
        }
    }

    fn drain_block_output(&mut self) -> Result<bool, SqlError> {
        if self.block_output.is_empty() {
            self.index_output_owner = None;
            return Ok(true);
        }
        let before = self.block_output.object_puts();
        let result = self
            .block_output
            .drain(&mut self.block_uploads, std::time::Instant::now());
        let requests = self.block_output.object_puts().saturating_sub(before);
        self.blocks
            .lock()
            .expect("block store lock poisoned")
            .base_mut()
            .record_puts(requests);
        let complete = result.map_err(upload_to_sql)?;
        if complete {
            self.block_output.consume();
            self.index_output_owner = None;
        }
        Ok(complete)
    }

    fn finish_immutable_upload(
        &mut self,
        ticket: crate::object_store::upload::Ticket,
    ) -> Result<(), SqlError> {
        self.uploads
            .finish(ticket)
            .map(|_| ())
            .map_err(upload_to_sql)
    }

    fn put_immutable(&mut self, key: &str, bytes: &[u8]) -> Result<(), SqlError> {
        let ticket = self
            .uploads
            .enqueue(
                key,
                bytes,
                crate::object_store::upload::Condition::ImmutableCreate,
            )
            .map_err(upload_to_sql)?;
        self.finish_immutable_upload(ticket)
    }

    /// Creates one immutable, named recovery point from the roots published by
    /// the preceding explicit checkpoint. The manifest object is the retention
    /// pin: garbage collection honors it even if the final completion marker
    /// is interrupted, while restore accepts only a complete backup.
    pub(crate) fn preflight_backup(&mut self, name: &str) -> Result<(), SqlError> {
        validate_backup_name(name)
            .map_err(|message| sql_err!(sqlstate::INVALID_PARAMETER_VALUE, "{message}"))?;
        let prefix = stack_format!(96, "backups/{name}/");
        let mut exists = false;
        self.client
            .list(prefix.as_str(), |_| exists = true)
            .map_err(object_store_to_sql)?;
        if exists {
            return Err(sql_err!(
                sqlstate::DUPLICATE_OBJECT,
                "backup \"{}\" already exists or has an incomplete creation",
                name
            ));
        }
        self.load_backup_names()?;
        if self.backup_scratch.len() == self.backup_scratch.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "backup roster exceeds max_backups ({})",
                self.backup_scratch.capacity()
            ));
        }
        Ok(())
    }

    pub(crate) fn create_backup(&mut self, name: &str) -> Result<u64, SqlError> {
        self.preflight_backup(name)?;

        self.client
            .get(MANIFEST_KEY, None)
            .map_err(object_store_to_sql)?;
        self.manifest_buf.clear();
        if !self.manifest_buf.append(self.client.body_bytes()) {
            return Err(manifest_full());
        }
        let floor = manifest_lsn(self.manifest_buf.readable())
            .map_err(|message| sql_err!(SQLSTATE_IO, "backup manifest: {message}"))?;
        let manifest_crc = crate::wal::crc32c::crc32c(self.manifest_buf.readable());

        let mut head = StackStr::<128>::new();
        match self.client.get(COMMIT_HEAD_KEY, None) {
            Ok(_) => {
                if !is_empty_commit_head(self.client.body_bytes())
                    .map_err(|message| sql_err!(SQLSTATE_IO, "{message}"))?
                {
                    use core::fmt::Write as _;
                    write!(
                        head,
                        "{}",
                        core::str::from_utf8(self.client.body_bytes())
                            .map_err(|_| sql_err!(SQLSTATE_IO, "commit-head is not UTF-8"))?
                    )
                    .map_err(|_| sql_err!(SQLSTATE_IO, "commit-head exceeds its format bound"))?;
                    parse_commit_head(head.as_str().as_bytes())
                        .map_err(|message| sql_err!(SQLSTATE_IO, "{message}"))?;
                }
            }
            Err(error) if error.is_not_found() => {}
            Err(error) => return Err(object_store_to_sql(error)),
        }
        let head_crc = crate::wal::crc32c::crc32c(head.as_str().as_bytes());
        // From the first durable write onward, a failed creation may leave a
        // manifest retention pin. Force the next collector to rediscover it.
        self.invalidate_backup_roster();
        let head_key = stack_format!(128, "backups/{name}/commit-head");
        self.put_immutable(head_key.as_str(), head.as_str().as_bytes())?;
        let manifest_key = stack_format!(128, "backups/{name}/manifest");
        match self.client.put(
            manifest_key.as_str(),
            self.manifest_buf.readable(),
            Precondition::IfNoneMatchAny,
        ) {
            Ok(_) => {}
            Err(error) => return Err(object_store_to_sql(error)),
        }
        let complete_key = stack_format!(128, "backups/{name}/complete");
        let created_at = crate::sql::datetime::now_micros();
        let complete = stack_format!(
            192,
            "{BACKUP_HEADER}\nlsn {floor}\ncreated_at {created_at}\nmanifest_crc {manifest_crc:08x}\nhead_crc {head_crc:08x}\nend\n"
        );
        self.put_immutable(complete_key.as_str(), complete.as_str().as_bytes())?;
        Ok(floor)
    }

    pub(crate) fn delete_backup(&mut self, name: &str) -> Result<bool, SqlError> {
        validate_backup_name(name)
            .map_err(|message| sql_err!(sqlstate::INVALID_PARAMETER_VALUE, "{message}"))?;
        match self.client.get(RESTORE_PENDING_KEY, None) {
            Ok(_) => {
                return Err(sql_err!(
                    sqlstate::OBJECT_IN_USE,
                    "cannot delete a backup while restore-pending exists"
                ));
            }
            Err(error) if error.is_not_found() => {}
            Err(error) => return Err(object_store_to_sql(error)),
        }
        let prefix = stack_format!(96, "backups/{name}/");
        let mut exists = false;
        self.client
            .list(prefix.as_str(), |_| exists = true)
            .map_err(object_store_to_sql)?;
        if !exists {
            return Ok(false);
        }
        // Deletion removes the manifest last. Any interrupted attempt must be
        // rediscovered before the next retention decision.
        self.invalidate_backup_roster();
        for suffix in ["complete", "commit-head", "manifest"] {
            let key = stack_format!(128, "backups/{name}/{suffix}");
            self.client
                .delete(key.as_str())
                .map_err(object_store_to_sql)?;
        }
        Ok(true)
    }

    /// Downloads and replays commit batches with records past `floor`, in
    /// ascending order, feeding each record to `apply`. The caller merges
    /// these with the local journal's records by LSN before applying:
    /// neither source alone spans the committed history (the journal may
    /// restart mid-history after a disk wipe or end early at a torn write,
    /// and the segments lack whatever a failed upload left journaled-only).
    /// The key roster uses startup-reserved scratch, so callers can also use
    /// this read-only path while serving a logical replication stream.
    #[inline(never)]
    pub(crate) fn replay_commit_batches(
        &mut self,
        floor: u64,
        mut apply: impl FnMut(u64, &[u8]) -> Result<(), SqlError>,
    ) -> Result<(), CheckpointSetupError> {
        self.commit_scratch.clear();
        let mut batch = self.commit_head;
        while let Some(current) = batch {
            if self.commit_scratch.len() == self.commit_scratch.capacity() {
                return Err(CheckpointSetupError::ObjectStore(format!(
                    "commit-head chain exceeds checkpoint_commit_batches ({})",
                    self.commit_scratch.capacity()
                )));
            }
            let descriptor_key = crate::stack_format!(
                72,
                "commits/{:020}-{:08x}.head",
                current.first_lsn,
                current.digest
            );
            self.client
                .get(descriptor_key.as_str(), None)
                .map_err(|error| {
                    CheckpointSetupError::ObjectStore(format!("get commit head: {error}"))
                })?;
            let (described, previous) = parse_commit_descriptor(self.client.body_bytes())
                .map_err(CheckpointSetupError::Corrupt)?;
            if described != current {
                return Err(CheckpointSetupError::Corrupt(
                    "commit descriptor does not match its head",
                ));
            }
            self.commit_scratch.push(crate::stack_format!(
                64,
                "commits/{:020}-{:08x}.batch",
                current.first_lsn,
                current.digest
            ));
            batch = previous;
            // A batch is identified by its first record, not its last. The
            // first batch at or before the floor may straddle it, so replay it
            // and let record framing discard covered records. Its predecessor
            // cannot contribute a record past this batch's first LSN.
            if current.first_lsn <= floor {
                break;
            }
        }
        for index in (0..self.commit_scratch.len()).rev() {
            let key = self.commit_scratch[index];
            // Ranged, buffer-sized windows: a segment is one committed WAL
            // batch, whose size is bounded by wal_buffer_bytes — which may
            // exceed the response buffer. An unranged GET would upload fine
            // and then be unrecoverable at cold start (ResponseTooLarge), so
            // the segment streams through the buffer instead; a partially
            // fetched record re-fetches from its own start.
            let mut offset = 0u64;
            loop {
                let to = offset + self.client.response_capacity() as u64 - 1;
                match self.client.get(
                    key.as_str(),
                    Some(ByteRange::new(offset, to).expect("nonempty WAL record range")),
                ) {
                    Ok(_) => {}
                    // Past the end of the object: the segment is fully read.
                    Err(ObjectError::Status { code: 416, .. }) => break,
                    Err(e) => {
                        return Err(CheckpointSetupError::ObjectStore(format!(
                            "get commit batch: {e}"
                        )));
                    }
                }
                let body = self.client.body_bytes();
                if body.is_empty() {
                    break;
                }
                // Records are the same framed format as the local journal.
                let consumed = replay_segment_bytes(body, floor, &mut apply)
                    .map_err(CheckpointSetupError::Replay)?;
                if consumed == 0 {
                    if body.len() < self.client.response_capacity() {
                        // A trailing partial record (torn upload tail): the
                        // local-journal replay rule — stop at the first
                        // invalid record — applies here too.
                        break;
                    }
                    return Err(CheckpointSetupError::ObjectStore(format!(
                        "commit record in {} exceeds object_store_response_bytes; raise it past wal_buffer_bytes",
                        key.as_str()
                    )));
                }
                offset += consumed as u64;
                if body.len() < self.client.response_capacity() {
                    break;
                }
            }
        }
        Ok(())
    }

    /// Reads the earliest whole committed transaction after `floor` from the
    /// retained object-store WAL. The transaction is rebuilt in the caller's
    /// fixed buffer using the ordinary journal frame layout, so live logical
    /// decoding and startup recovery validate the same record boundary.
    pub(crate) fn next_committed_wal_transaction(
        &mut self,
        floor: u64,
        ceiling: u64,
        scratch: &mut FixedBuf,
        mut apply: impl FnMut(u64, &[u8]) -> Result<(), SqlError>,
    ) -> Result<Option<u64>, SqlError> {
        scratch.clear();
        let mut complete_lsn = None;
        self.replay_commit_batches(floor, |lsn, record| {
            if complete_lsn.is_some() || lsn > ceiling {
                return Ok(());
            }
            append_uploaded_wal_record(scratch, lsn, record)?;
            if matches!(
                crate::wal::decode_record(record),
                Some(crate::wal::WalOp::Commit { .. })
            ) {
                complete_lsn = Some(lsn);
            }
            Ok(())
        })
        .map_err(|error| sql_err!(sqlstate::IO_ERROR, "logical WAL read failed: {error}"))?;
        let Some(end_lsn) = complete_lsn else {
            scratch.clear();
            return Ok(None);
        };
        apply(end_lsn, scratch.readable())?;
        scratch.clear();
        Ok(Some(end_lsn))
    }

    /// Schedules deletion of commit objects covered by a durable manifest.
    /// Finish the current boundary before the latest newer one. A lower
    /// retention boundary cancels pending work before any further deletion.
    pub(crate) fn schedule_commit_prune(&mut self, up_to_lsn: u64) {
        if let Some(current) = self.commit_prune_through
            && up_to_lsn > current
        {
            self.commit_prune_next = Some(up_to_lsn);
            return;
        }
        self.commit_prune_next = None;
        let floor = self
            .commit_prune_through
            .map_or(up_to_lsn, |pending| pending.min(up_to_lsn));
        if self
            .commit_prune_through
            .is_some_and(|pending| floor < pending)
        {
            self.commit_prune = CommitPruneProgress::Unloaded;
            self.metadata_io.cancel();
            self.commit_garbage_scratch.clear();
        }
        self.commit_prune_through = Some(floor);
    }

    fn load_backup_names(&mut self) -> Result<(), SqlError> {
        self.backup_roster_loaded = false;
        self.backup_scratch.clear();
        let names = &mut self.backup_scratch;
        let mut overflow = false;
        self.client
            .list("backups/", |key| {
                let Some(name) = key
                    .strip_prefix("backups/")
                    .and_then(|rest| rest.strip_suffix("/manifest"))
                else {
                    return;
                };
                if validate_backup_name(name).is_err() {
                    overflow = true;
                    return;
                }
                if names.len() == names.capacity() {
                    overflow = true;
                } else {
                    names.push(StackStr::from_str(name));
                }
            })
            .map_err(object_store_to_sql)?;
        self.backup_scratch
            .sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
        self.backup_scratch
            .dedup_by(|left, right| left.as_str() == right.as_str());
        if overflow {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "backup roster exceeds max_backups ({}) or contains an invalid backup name",
                self.backup_scratch.capacity()
            ));
        }
        self.backup_roster_loaded = true;
        Ok(())
    }

    fn ensure_backup_names_loaded(&mut self) -> Result<(), SqlError> {
        if !self.backup_roster_loaded {
            self.load_backup_names()?;
        }
        Ok(())
    }

    fn invalidate_backup_roster(&mut self) {
        self.backup_roster_loaded = false;
        self.prune_backup_roster_loaded = false;
        if !matches!(self.commit_prune, CommitPruneProgress::Unloaded) {
            self.commit_prune = CommitPruneProgress::Unloaded;
            self.metadata_io.cancel();
            self.commit_garbage_scratch.clear();
        }
    }

    fn begin_prune_boundary(&mut self, up_to_lsn: u64, oldest: Option<u64>) {
        self.commit_garbage_scratch.clear();
        self.commit_prune = CommitPruneProgress::FindBoundary {
            floor: oldest.map_or(up_to_lsn, |backup| up_to_lsn.min(backup)),
            cursor: self.commit_head,
        };
    }

    /// Keep the published chain's covered boundary. Each turn advances a backup
    /// or commit-list page, retention manifest, descriptor or deletion batch.
    fn prune_commit_batch(&mut self, up_to_lsn: u64) -> Result<bool, SqlError> {
        #[cfg(feature = "checkpoint-profile")]
        let started = checkpoint_profile_start();
        #[cfg(feature = "checkpoint-profile")]
        let before = self
            .blocks
            .lock()
            .expect("block store lock poisoned")
            .io_stats();
        let mut deleted = 0;
        let result = self.advance_commit_prune(up_to_lsn, &mut deleted);
        #[cfg(feature = "checkpoint-profile")]
        profile_checkpoint_phase(
            "commit_prune",
            up_to_lsn,
            None,
            started,
            before,
            self.blocks
                .lock()
                .expect("block store lock poisoned")
                .io_stats(),
            deleted,
        );
        result
    }

    fn advance_commit_prune(
        &mut self,
        up_to_lsn: u64,
        deleted: &mut usize,
    ) -> Result<bool, SqlError> {
        match self.commit_prune {
            CommitPruneProgress::Unloaded => {
                self.commit_garbage_scratch.clear();
                if self.prune_backup_roster_loaded {
                    if self.prune_backup_scratch.is_empty() {
                        self.begin_prune_boundary(up_to_lsn, None);
                        return Ok(false);
                    }
                    self.commit_prune = CommitPruneProgress::BackupFloors {
                        index: 0,
                        oldest: None,
                    };
                    return Ok(false);
                }
                self.prune_backup_scratch.clear();
                self.commit_prune = CommitPruneProgress::Backups {
                    cursor: ListCursor::START,
                };
                self.advance_commit_prune(up_to_lsn, deleted)
            }
            CommitPruneProgress::Backups { cursor } => {
                let names = &mut self.prune_backup_scratch;
                let mut invalid = false;
                let Poll::Ready(next) = self.metadata_io.list("backups/", cursor, |key| {
                    let Some(name) = key
                        .strip_prefix("backups/")
                        .and_then(|key| key.strip_suffix("/manifest"))
                    else {
                        return;
                    };
                    if validate_backup_name(name).is_err() || names.len() == names.capacity() {
                        invalid = true;
                    } else {
                        names.push(StackStr::from_str(name));
                    }
                })?
                else {
                    return Ok(false);
                };
                if invalid {
                    self.commit_prune = CommitPruneProgress::Unloaded;
                    self.prune_backup_scratch.clear();
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "backup roster exceeds max_backups ({}) or contains an invalid backup name",
                        self.prune_backup_scratch.capacity()
                    ));
                }
                if let Some(cursor) = next {
                    self.commit_prune = CommitPruneProgress::Backups { cursor };
                } else {
                    self.prune_backup_scratch
                        .sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
                    self.prune_backup_roster_loaded = true;
                    if self.prune_backup_scratch.is_empty() {
                        self.begin_prune_boundary(up_to_lsn, None);
                    } else {
                        self.commit_prune = CommitPruneProgress::BackupFloors {
                            index: 0,
                            oldest: None,
                        };
                    }
                }
                Ok(false)
            }
            CommitPruneProgress::BackupFloors { index, oldest } => {
                let key =
                    stack_format!(128, "backups/{}/manifest", self.prune_backup_scratch[index]);
                let Poll::Ready(bytes) = self.metadata_io.get(key.as_str())? else {
                    return Ok(false);
                };
                let floor = manifest_commit_retention_floor(bytes)
                    .map_err(|message| sql_err!(SQLSTATE_IO, "backup manifest: {message}"))?;
                let oldest = Some(oldest.map_or(floor, |current| current.min(floor)));
                if index + 1 == self.prune_backup_scratch.len() {
                    self.begin_prune_boundary(up_to_lsn, oldest);
                } else {
                    self.commit_prune = CommitPruneProgress::BackupFloors {
                        index: index + 1,
                        oldest,
                    };
                }
                Ok(false)
            }
            CommitPruneProgress::FindBoundary { floor, cursor } => {
                if let Some(batch) = cursor {
                    let key = stack_format!(
                        72,
                        "commits/{:020}-{:08x}.head",
                        batch.first_lsn,
                        batch.digest
                    );
                    let Poll::Ready(bytes) = self.metadata_io.get(key.as_str())? else {
                        return Ok(false);
                    };
                    let (described, previous) =
                        parse_commit_descriptor(bytes).map_err(|message| {
                            sql_err!(sqlstate::IO_ERROR, "commit prune: {message}")
                        })?;
                    if described != batch
                        || previous.is_some_and(|previous| previous.first_lsn >= batch.first_lsn)
                    {
                        return Err(sql_err!(
                            sqlstate::IO_ERROR,
                            "commit prune descriptor does not match its ordered chain"
                        ));
                    }
                    self.commit_prune = if batch.first_lsn <= floor {
                        CommitPruneProgress::List {
                            floor,
                            boundary: Some(batch),
                            cursor: ListCursor::START,
                        }
                    } else {
                        CommitPruneProgress::FindBoundary {
                            floor,
                            cursor: previous,
                        }
                    };
                } else {
                    self.commit_prune = CommitPruneProgress::List {
                        floor,
                        boundary: cursor,
                        cursor: ListCursor::START,
                    };
                }
                Ok(false)
            }
            CommitPruneProgress::List {
                floor,
                boundary,
                cursor,
            } => {
                let mut overflow = false;
                // Canonical names are ordered by their fixed-width first LSN.
                // New commits must not extend this completed checkpoint's scan.
                let last_candidate = stack_format!(64, "commits/{floor:020}-ffffffff.head");
                let mut past_floor = false;
                let doomed = &mut self.commit_garbage_scratch;
                let Poll::Ready(next) = self.metadata_io.list("commits/", cursor, |key| {
                    if key > last_candidate.as_str() {
                        past_floor = true;
                        return;
                    }
                    let is_doomed = commit_object_identity(key).is_some_and(|identity| {
                        identity.first_lsn <= floor && Some(identity) != boundary
                    });
                    if is_doomed {
                        if doomed.len() < doomed.capacity() {
                            doomed.push(stack_format!(64, "{}", key));
                        } else {
                            overflow = true;
                        }
                    }
                })?
                else {
                    return Ok(false);
                };
                self.commit_prune = match next {
                    Some(cursor) if !overflow && !past_floor => CommitPruneProgress::List {
                        floor,
                        boundary,
                        cursor,
                    },
                    _ => CommitPruneProgress::Delete {
                        floor,
                        boundary,
                        overflow,
                    },
                };
                Ok(false)
            }
            CommitPruneProgress::Delete {
                floor,
                boundary,
                overflow,
            } => {
                for _ in 0..self
                    .delete_objects_per_beat
                    .min(self.commit_garbage_scratch.len())
                {
                    let key = *self
                        .commit_garbage_scratch
                        .last()
                        .expect("nonempty staged commit deletion batch");
                    if self.metadata_io.delete(key.as_str())?.is_pending() {
                        return Ok(false);
                    }
                    self.commit_garbage_scratch.pop();
                    *deleted += 1;
                }
                if !self.commit_garbage_scratch.is_empty() {
                    return Ok(false);
                }
                self.commit_prune = if overflow {
                    CommitPruneProgress::List {
                        floor,
                        boundary,
                        cursor: ListCursor::START,
                    }
                } else {
                    CommitPruneProgress::Unloaded
                };
                Ok(!overflow)
            }
        }
    }

    /// Cold start: loads the manifest (if any) and rehydrates every SST
    /// into storage. Returns the manifest LSN — the WAL replay floor.
    /// Startup only (allocates freely while parsing).
    pub(crate) fn load_into(&mut self, storage: &mut Storage) -> Result<u64, CheckpointSetupError> {
        for (key, operation) in [
            (RESTORE_PENDING_KEY, "restore"),
            (EXPORT_PENDING_KEY, "backup export"),
        ] {
            match self.client.get(key, None) {
                Ok(_) => {
                    return Err(CheckpointSetupError::ObjectStore(format!(
                        "{key} exists; rerun the offline {operation} operation"
                    )));
                }
                Err(error) if error.is_not_found() => {}
                Err(error) => {
                    return Err(CheckpointSetupError::ObjectStore(format!(
                        "load {key}: {error}"
                    )));
                }
            }
        }
        self.manifest_etag = read_mutable_root(
            &mut self.client,
            MutableRoot::Manifest,
            &mut self.manifest_buf,
        )?;
        self.manifest_present = false;
        let floor = if self.manifest_etag.is_some() {
            if is_empty_manifest(self.manifest_buf.readable())
                .map_err(CheckpointSetupError::Corrupt)?
            {
                0
            } else {
                let text = core::str::from_utf8(self.manifest_buf.readable())
                    .map_err(|_| CheckpointSetupError::Corrupt("manifest is not UTF-8"))?
                    .to_owned();
                let floor = self.load_manifest_text(storage, &text)?;
                self.manifest_present = true;
                floor
            }
        } else {
            0
        };
        self.commit_head_etag = read_mutable_root(
            &mut self.client,
            MutableRoot::CommitHead,
            &mut self.root_buf,
        )?;
        self.commit_head = if self.commit_head_etag.is_some()
            && !is_empty_commit_head(self.root_buf.readable())
                .map_err(CheckpointSetupError::Corrupt)?
        {
            Some(
                parse_commit_head(self.root_buf.readable())
                    .map_err(CheckpointSetupError::Corrupt)?
                    .1,
            )
        } else {
            None
        };
        if self.commit_head.is_none() {
            let mut has_orphaned_batches = false;
            self.client
                .list("commits/", |_| has_orphaned_batches = true)
                .map_err(|error| {
                    CheckpointSetupError::ObjectStore(format!(
                        "list orphaned commit batches: {error}"
                    ))
                })?;
            if has_orphaned_batches {
                return Err(CheckpointSetupError::Corrupt(
                    "commit batches exist without a commit-head",
                ));
            }
        }
        // Catalog and immutable references are valid before changing any root.
        // Promotion checks both captured versions again after fencing: a racing
        // writer requires a fresh startup, never publication from stale storage.
        self.claim_writer()?;
        Ok(floor)
    }

    #[inline(never)]
    fn load_manifest_text(
        &mut self,
        storage: &mut Storage,
        text: &str,
    ) -> Result<u64, CheckpointSetupError> {
        let metadata = parse_manifest_metadata(text).map_err(CheckpointSetupError::Corrupt)?;
        storage.begin_object_acl_restore().map_err(|_| {
            CheckpointSetupError::Corrupt("object ACL restoration requires initial catalog state")
        })?;
        let lsn = metadata.lsn;
        let next_rowid = metadata.next_rowid;
        storage.restore_catalog_identity_sequence(metadata.catalog_sequence);
        storage
            .restore_role_identity_sequence(metadata.role_sequence)
            .map_err(|_| CheckpointSetupError::Corrupt("invalid role identity allocator"))?;
        storage.restore_next_large_object_oid(metadata.next_large_object_oid);
        storage.clear_schemas_for_restore();
        let lines = text.lines().skip(1);
        let mut database_identities = Vec::with_capacity(storage.database_count());
        let mut system_setting_names = Vec::with_capacity(crate::storage::MAX_SYSTEM_SETTINGS);
        // Validated source table positions; current records retain their original slots.
        let mut slot_of: Vec<Option<usize>> = Vec::new();
        // (source slot, creation stamp, definition, columns seen, sequence values)
        let mut pending_def: Option<(
            usize,
            u64,
            TableDef,
            usize,
            [i64; crate::storage::MAX_COLUMNS],
        )> = None;
        // (mindex, list index, count, crc, handle) — the block-grid form.
        let mut bssts: Vec<(usize, usize, u64, u32, Option<SstHandle>)> = Vec::new();
        let mut value_indexes: Vec<LoadedValueIndex> = Vec::new();
        let mut table_statistics: Vec<(usize, crate::storage::TableStatistics)> = Vec::new();
        let mut relation_index_hints: Vec<(usize, bool)> = Vec::new();
        struct LoadedExtendedStatistics {
            database: crate::storage::DatabaseOid,
            created_at: u64,
            table_index: usize,
            schema: crate::storage::SqlName,
            name: crate::storage::SqlName,
            target: Option<u16>,
            kinds: crate::sql::ast::StatisticsKinds,
            expression_only: bool,
            keys: [crate::storage::ExtendedStatisticsKey;
                crate::storage::MAX_EXTENDED_STATISTICS_KEYS],
            n_keys: u8,
            data: crate::storage::ExtendedStatisticsData,
        }
        let mut extended_statistics: Vec<LoadedExtendedStatistics> = Vec::new();
        let mut saw_end = false;

        for line in lines {
            let mut words = line.split(' ');
            match words.next() {
                Some("types_end") => {
                    storage.finish_recovered_type_sources().map_err(|error| {
                        CheckpointSetupError::ObjectStore(format!(
                            "manifest type sources rejected: {}",
                            error.message.as_str()
                        ))
                    })?;
                }
                Some(
                    "lsn"
                    | "next_rowid"
                    | "latest_transaction_id"
                    | "next_lo_oid"
                    | "catalog_sequence"
                    | "role_sequence"
                    | "writer",
                ) => {}
                Some("transaction_status") => {
                    let transaction_id = parse_field(words.next(), "transaction_status")?;
                    let committed = match words.next() {
                        Some("committed") => true,
                        Some("aborted") => false,
                        _ => {
                            return Err(CheckpointSetupError::Corrupt(
                                "invalid retained transaction status",
                            ));
                        }
                    };
                    if words.next().is_some()
                        || storage
                            .restore_transaction_status(transaction_id, committed)
                            .is_err()
                    {
                        return Err(CheckpointSetupError::Corrupt(
                            "invalid retained transaction status",
                        ));
                    }
                }
                Some("dbctx") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let oid: i32 = parse_field(words.next(), "database context")?;
                    let database = crate::storage::DatabaseOid::parse(oid)
                        .ok_or(CheckpointSetupError::Corrupt("invalid database context"))?;
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt(
                            "trailing database context fields",
                        ));
                    }
                    storage
                        .select_database_for_recovery(database)
                        .map_err(|_| CheckpointSetupError::Corrupt("unknown database context"))?;
                }
                Some("am") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let identity = parse_catalog_position(&mut words, metadata.catalog_sequence)?;
                    let name = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("access method name missing"))?,
                    )?;
                    let handler: u8 = parse_field(words.next(), "access method handler")?;
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt(
                            "invalid access method record",
                        ));
                    }
                    storage
                        .replay_access_method(
                            identity,
                            crate::storage::AccessMethodDefinition {
                                name: sql_name(&name)?,
                                handler: crate::storage::TableAccessMethodHandler::from_code(
                                    handler,
                                )
                                .ok_or(
                                    CheckpointSetupError::Corrupt("invalid access method handler"),
                                )?,
                            },
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest access method rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("table2") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let mindex: usize = parse_field(words.next(), "table index")?;
                    let created_at: u64 = parse_field(words.next(), "table source creation")?;
                    if created_at > metadata.catalog_sequence {
                        return Err(CheckpointSetupError::Corrupt(
                            "table source exceeds saved identity allocator",
                        ));
                    }
                    let n_cols: usize = parse_field(words.next(), "table columns")?;
                    if n_cols > MAX_RELATION_COLUMNS {
                        return Err(CheckpointSetupError::Corrupt("too many columns"));
                    }
                    let has_toast = parse_bool_field(words.next(), "table toast relation")?;
                    let has_rules = parse_bool_field(words.next(), "table rewrite rules")?;
                    let kind: u8 = parse_field(words.next(), "table kind")?;
                    let kind = match kind {
                        0 => crate::storage::TableKind::Local,
                        1 => crate::storage::TableKind::Foreign,
                        _ => return Err(CheckpointSetupError::Corrupt("invalid table kind")),
                    };
                    let tablespace: u16 = parse_field(words.next(), "table tablespace")?;
                    let access_method: i32 = parse_field(words.next(), "table access method")?;
                    let access_method = if access_method == 2 {
                        crate::storage::TableAccessMethod::Heap
                    } else {
                        crate::storage::TableAccessMethod::Catalog(
                            crate::storage::AccessMethodOid::parse(access_method).ok_or(
                                CheckpointSetupError::Corrupt("invalid table access method"),
                            )?,
                        )
                    };
                    if storage.table_access_method_name(access_method, 0).is_none() {
                        return Err(CheckpointSetupError::Corrupt(
                            "table references an unknown access method",
                        ));
                    }
                    let inheritance_count: usize =
                        parse_field(words.next(), "table inheritance count")?;
                    if inheritance_count > crate::storage::MAX_TABLE_INHERITANCE_PARENTS {
                        return Err(CheckpointSetupError::Corrupt(
                            "too many table inheritance parents",
                        ));
                    }
                    let mut inheritance = crate::storage::TableInheritance::NONE;
                    for _ in 0..inheritance_count {
                        let parent: u16 = parse_field(words.next(), "table inheritance parent")?;
                        inheritance.append(usize::from(parent)).map_err(|_| {
                            CheckpointSetupError::Corrupt("invalid table inheritance")
                        })?;
                    }
                    let membership = words.next().ok_or(CheckpointSetupError::Corrupt(
                        "missing typed-table membership",
                    ))?;
                    let type_membership = if membership == "-1" {
                        crate::storage::TableTypeMembership::None
                    } else {
                        let reference = crate::storage::TypeReference::from_storage(membership)
                            .map_err(|_| {
                                CheckpointSetupError::Corrupt("invalid typed-table membership")
                            })?;
                        let slot =
                            reference
                                .composite_slot()
                                .ok_or(CheckpointSetupError::Corrupt(
                                    "invalid typed-table membership",
                                ))?;
                        if slot >= storage.composite_count() {
                            return Err(CheckpointSetupError::Corrupt(
                                "typed-table membership out of range",
                            ));
                        }
                        crate::storage::TableTypeMembership::Composite(reference)
                    };
                    let fillfactor: u8 = parse_field(words.next(), "table fillfactor")?;
                    if fillfactor != 0 && !(10..=100).contains(&fillfactor) {
                        return Err(CheckpointSetupError::Corrupt("invalid table fillfactor"));
                    }
                    let name = rest_of(line, 12 + inheritance_count)?;
                    let def = TableDef {
                        // The current format omits `tsch` for the public schema.
                        schema: sql_name("public")?,
                        name: sql_name(name)?,
                        columns: [empty_column(); MAX_COLUMNS],
                        n_columns: n_cols,
                        attributes: crate::storage::column_attributes::ColumnAttributes {
                            count: u16::MAX,
                            ..crate::storage::column_attributes::ColumnAttributes::EMPTY
                        },
                        has_toast,
                        has_rules,
                        kind,
                        tablespace,
                        access_method,
                        inheritance,
                        type_membership,
                        storage_options: crate::storage::TableStorageOptions {
                            fillfactor: (fillfactor != 0).then_some(fillfactor),
                        },
                        ..TableDef::empty()
                    };
                    pending_def = Some((
                        mindex,
                        created_at,
                        def,
                        0,
                        [0i64; crate::storage::MAX_COLUMNS],
                    ));
                }
                Some("attributes2") => {
                    use crate::storage::column_attributes::{AttributeSlot, ColumnAttributes};
                    let Some((_, _, def, _, _)) = pending_def.as_mut() else {
                        return Err(CheckpointSetupError::Corrupt("attributes outside table"));
                    };
                    let number: usize = parse_field(words.next(), "attribute number")?;
                    if number == 0 {
                        let count: u16 = parse_field(words.next(), "attribute count")?;
                        if def.attributes.count != u16::MAX
                            || usize::from(count) > crate::storage::MAX_RELATION_COLUMNS
                            || words.next().is_some()
                        {
                            return Err(CheckpointSetupError::Corrupt(
                                "invalid column attribute count",
                            ));
                        }
                        def.attributes = ColumnAttributes {
                            count,
                            ..ColumnAttributes::EMPTY
                        };
                    } else {
                        if def.attributes.count == u16::MAX
                            || number > usize::from(def.attributes.count)
                            || def.attributes.slots[number - 1] != AttributeSlot::Empty
                        {
                            return Err(CheckpointSetupError::Corrupt(
                                "invalid column attribute number",
                            ));
                        }
                        let encoded = words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("missing column attribute"))?;
                        if encoded.len() != 16
                            || !encoded.bytes().all(|byte| byte.is_ascii_hexdigit())
                            || words.next().is_some()
                        {
                            return Err(CheckpointSetupError::Corrupt(
                                "invalid column attribute encoding",
                            ));
                        }
                        let mut bytes = [0u8; 8];
                        for (position, byte) in bytes.iter_mut().enumerate() {
                            *byte =
                                u8::from_str_radix(&encoded[position * 2..position * 2 + 2], 16)
                                    .map_err(|_| {
                                        CheckpointSetupError::Corrupt(
                                            "invalid column attribute encoding",
                                        )
                                    })?;
                        }
                        def.attributes.slots[number - 1] = AttributeSlot::decode(bytes)
                            .ok_or(CheckpointSetupError::Corrupt("invalid column attribute"))?;
                    }
                }
                Some("persist") => {
                    let Some((_, _, def, _, _)) = pending_def.as_mut() else {
                        return Err(CheckpointSetupError::Corrupt("persist outside table"));
                    };
                    let code = words
                        .next()
                        .and_then(|word| word.as_bytes().first().copied())
                        .and_then(crate::storage::RelationPersistence::from_code)
                        .ok_or(CheckpointSetupError::Corrupt("invalid table persistence"))?;
                    if code == crate::storage::RelationPersistence::Temporary
                        || words.next().is_some()
                    {
                        return Err(CheckpointSetupError::Corrupt(
                            "invalid durable table persistence",
                        ));
                    }
                    def.persistence = code;
                }
                Some("col4") => {
                    let Some((_, _, def, seen, _)) = pending_def.as_mut() else {
                        return Err(CheckpointSetupError::Corrupt("col outside table"));
                    };
                    let type_code: u8 = parse_field(words.next(), "col type")?;
                    let not_null: u8 = parse_field(words.next(), "col notnull")?;
                    let not_null_inheritable: u8 =
                        parse_field(words.next(), "col not-null inheritable")?;
                    if not_null_inheritable > 1 {
                        return Err(CheckpointSetupError::Corrupt(
                            "invalid NOT NULL inheritance",
                        ));
                    }
                    let not_null_text = match words.next() {
                        Some("0") => String::new(),
                        Some(value) => decode_hex_name(value)?,
                        None => {
                            return Err(CheckpointSetupError::Corrupt("col NOT NULL name missing"));
                        }
                    };
                    let not_null_name = if not_null_text.is_empty() {
                        None
                    } else {
                        Some(sql_name(&not_null_text)?)
                    };
                    if (not_null & 3 != 0) != not_null_name.is_some() {
                        return Err(CheckpointSetupError::Corrupt(
                            "invalid column NOT NULL name",
                        ));
                    }
                    let type_mod: i32 = parse_field(words.next(), "col typmod")?;
                    let default_hex = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("col default missing"))?;
                    let dexpr_hex = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("col default_expr missing"))?;
                    let default_expr = if dexpr_hex == "0" {
                        None
                    } else {
                        Some(crate::util::StackStr::from_str(&decode_hex_name(
                            dexpr_hex,
                        )?))
                    };
                    let auto_increment_step: i64 = parse_field(words.next(), "col step")?;
                    let statistics_target: i16 =
                        parse_field(words.next(), "col statistics target")?;
                    if !(-1..=10_000).contains(&statistics_target) {
                        return Err(CheckpointSetupError::Corrupt("column statistics target"));
                    }
                    let statistics_option_flags: u8 =
                        parse_field(words.next(), "column statistics option flags")?;
                    if statistics_option_flags & !3 != 0 {
                        return Err(CheckpointSetupError::Corrupt(
                            "column statistics option flags",
                        ));
                    }
                    let n_distinct_bits: u32 = parse_field(words.next(), "column n_distinct")?;
                    let n_distinct_inherited_bits: u32 =
                        parse_field(words.next(), "column n_distinct inherited")?;
                    let statistics_options = crate::sql::ast::ColumnStatisticsOptions::decode(
                        statistics_option_flags,
                        n_distinct_bits,
                        n_distinct_inherited_bits,
                    )
                    .ok_or(CheckpointSetupError::Corrupt("column statistics options"))?;
                    let storage = crate::sql::ast::ColumnStorage::from_code(parse_field(
                        words.next(),
                        "col storage",
                    )?)
                    .ok_or(CheckpointSetupError::Corrupt("unknown column storage"))?;
                    let compression = crate::sql::ast::ColumnCompression::from_code(parse_field(
                        words.next(),
                        "col compression",
                    )?)
                    .ok_or(CheckpointSetupError::Corrupt("unknown column compression"))?;
                    let ctype = ColType::from_code(type_code)
                        .ok_or(CheckpointSetupError::Corrupt("unknown column type code"))?;
                    let collation = crate::sql::ast::Collation::from_code(parse_field::<u32>(
                        words.next(),
                        "col collation",
                    )?)
                    .ok_or(CheckpointSetupError::Corrupt("unknown column collation"))?;
                    let schema_hex = words.next().ok_or(CheckpointSetupError::Corrupt(
                        "col user type schema missing",
                    ))?;
                    let user_type_schema = if schema_hex == "0" {
                        None
                    } else {
                        Some(sql_name(&decode_hex_name(schema_hex)?)?)
                    };
                    let domain_hex = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("col domain missing"))?;
                    let domain = if domain_hex == "0" {
                        None
                    } else {
                        Some(decode_hex_name(domain_hex)?)
                    };
                    let user_type = match (user_type_schema, domain) {
                        (None, None) => None,
                        (Some(schema), Some(name)) => Some(
                            crate::storage::UserTypeName::from_storage(schema, &name).map_err(
                                |_| CheckpointSetupError::Corrupt("invalid column type identity"),
                            )?,
                        ),
                        _ => {
                            return Err(CheckpointSetupError::Corrupt(
                                "column user type identity is incomplete",
                            ));
                        }
                    };
                    let name = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("column name missing"))?,
                    )?;
                    if name.is_empty() || words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt(
                            "invalid column name or trailing fields",
                        ));
                    }
                    if *seen >= def.n_columns {
                        return Err(CheckpointSetupError::Corrupt("too many col lines"));
                    }
                    let default = ColumnDefault::from_parts(
                        default_from_hex(default_hex)?,
                        default_expr,
                        not_null & 32 != 0,
                    )
                    .ok_or(CheckpointSetupError::Corrupt(
                        "invalid column default state",
                    ))?;
                    def.not_null_names[*seen] = not_null_name;
                    def.columns[*seen] = ColumnMeta {
                        name: sql_name(&name)?,
                        user_type,
                        ctype,
                        type_mod,
                        collation,
                        storage,
                        compression,
                        not_null: crate::storage::NotNullOrigin::from_code(not_null & 3)
                            .ok_or(CheckpointSetupError::Corrupt("invalid NOT NULL provenance"))?,
                        not_null_inheritable: not_null_inheritable != 0,
                        unique: not_null & 4 != 0,
                        primary: not_null & 8 != 0,
                        auto_increment: not_null & 16 != 0,
                        default,
                        is_identity: not_null & 64 != 0,
                        identity_always: not_null & 128 != 0,
                        auto_increment_step,
                        statistics_target,
                        statistics_options,
                    };
                    *seen += 1;
                }
                Some("hidx") => {
                    let table_index: usize = parse_field(words.next(), "hidx table index")?;
                    let has_index = match words.next() {
                        Some("0") => false,
                        Some("1") => true,
                        _ => {
                            return Err(CheckpointSetupError::Corrupt(
                                "invalid relation index hint",
                            ));
                        }
                    };
                    if words.next().is_some()
                        || relation_index_hints
                            .iter()
                            .any(|(index, _)| *index == table_index)
                    {
                        return Err(CheckpointSetupError::Corrupt(
                            "duplicate or trailing relation index hint",
                        ));
                    }
                    relation_index_hints.push((table_index, has_index));
                }
                Some("stats") => {
                    let table_index: usize = parse_field(words.next(), "stats table index")?;
                    let rows: u64 = parse_field(words.next(), "stats rows")?;
                    let average_row_width: u32 =
                        parse_field(words.next(), "stats average row width")?;
                    let analyzed_generation: u64 =
                        parse_field(words.next(), "stats analyzed generation")?;
                    if words.next().is_some()
                        || table_statistics
                            .iter()
                            .any(|(existing, _)| *existing == table_index)
                    {
                        return Err(CheckpointSetupError::Corrupt(
                            "duplicate or malformed table statistics",
                        ));
                    }
                    table_statistics.push((
                        table_index,
                        crate::storage::TableStatistics {
                            valid: true,
                            rows,
                            average_row_width,
                            analyzed_generation,
                            columns: [crate::storage::ColumnStatistics::EMPTY;
                                crate::storage::MAX_COLUMNS],
                        },
                    ));
                }
                Some("cstat") => {
                    let table_index: usize = parse_field(words.next(), "cstat table index")?;
                    let column: usize = parse_field(words.next(), "cstat column")?;
                    if column >= crate::storage::MAX_COLUMNS {
                        return Err(CheckpointSetupError::Corrupt(
                            "statistics column out of range",
                        ));
                    }
                    let null_fraction_ppm: u32 = parse_field(words.next(), "cstat null fraction")?;
                    if null_fraction_ppm > 1_000_000 {
                        return Err(CheckpointSetupError::Corrupt(
                            "statistics null fraction out of range",
                        ));
                    }
                    let distinct_values: u64 = parse_field(words.next(), "cstat distinct values")?;
                    let distinct_fraction_ppm: u32 =
                        parse_field(words.next(), "cstat distinct fraction")?;
                    if distinct_fraction_ppm > 1_000_000 {
                        return Err(CheckpointSetupError::Corrupt(
                            "statistics distinct fraction out of range",
                        ));
                    }
                    let average_width: u32 = parse_field(words.next(), "cstat average width")?;
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt("malformed column statistics"));
                    }
                    let Some((_, statistics)) = table_statistics
                        .iter_mut()
                        .find(|(existing, _)| *existing == table_index)
                    else {
                        return Err(CheckpointSetupError::Corrupt(
                            "column statistics precede table statistics",
                        ));
                    };
                    if statistics.columns[column].valid {
                        return Err(CheckpointSetupError::Corrupt("duplicate column statistics"));
                    }
                    statistics.columns[column] = crate::storage::ColumnStatistics {
                        valid: true,
                        null_fraction_ppm,
                        distinct_values,
                        distinct_fraction_ppm,
                        average_width,
                    };
                }
                Some("estat") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let created_at: u64 = parse_field(words.next(), "estat identity")?;
                    let table_index: usize = parse_field(words.next(), "estat table")?;
                    let target_raw: i32 = parse_field(words.next(), "estat target")?;
                    let target = match target_raw {
                        -1 => None,
                        0..=10_000 => Some(target_raw as u16),
                        _ => {
                            return Err(CheckpointSetupError::Corrupt("estat target out of range"));
                        }
                    };
                    let kind_code: u8 = parse_field(words.next(), "estat kinds")?;
                    let expression_only =
                        match parse_field::<u8>(words.next(), "estat expression kind")? {
                            0 => false,
                            1 => true,
                            _ => {
                                return Err(CheckpointSetupError::Corrupt(
                                    "invalid estat expression kind",
                                ));
                            }
                        };
                    let kinds = if expression_only && kind_code == 0 {
                        crate::sql::ast::StatisticsKinds::EXPRESSION
                    } else if !expression_only {
                        crate::sql::ast::StatisticsKinds::from_code(kind_code)
                            .ok_or(CheckpointSetupError::Corrupt("invalid estat kinds"))?
                    } else {
                        return Err(CheckpointSetupError::Corrupt("invalid estat kind state"));
                    };
                    let n_keys: usize = parse_field(words.next(), "estat key count")?;
                    if n_keys == 0 || n_keys > crate::storage::MAX_EXTENDED_STATISTICS_KEYS {
                        return Err(CheckpointSetupError::Corrupt("invalid estat key count"));
                    }
                    let schema = sql_name(&decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("estat schema missing"))?,
                    )?)?;
                    let name = sql_name(&decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("estat name missing"))?,
                    )?)?;
                    let mut keys = [crate::storage::ExtendedStatisticsKey::Column(
                        crate::storage::SqlName::EMPTY,
                    );
                        crate::storage::MAX_EXTENDED_STATISTICS_KEYS];
                    for key in &mut keys[..n_keys] {
                        let token = words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("estat key missing"))?;
                        if let Some(column) = token.strip_prefix('c') {
                            *key = crate::storage::ExtendedStatisticsKey::Column(sql_name(
                                &decode_hex_name(column)?,
                            )?);
                        } else if let Some(expression) = token.strip_prefix('e') {
                            let decoded = decode_hex_name(expression)?;
                            let source = crate::util::StackStr::from_str(&decoded);
                            if source.is_truncated() {
                                return Err(CheckpointSetupError::Corrupt(
                                    "estat expression is too long",
                                ));
                            }
                            *key = crate::storage::ExtendedStatisticsKey::Expression(source);
                        } else {
                            return Err(CheckpointSetupError::Corrupt("invalid estat key"));
                        }
                    }
                    if words.next().is_some()
                        || extended_statistics.iter().any(|statistics| {
                            statistics.database == storage.current_database_oid()
                                && statistics.created_at == created_at
                        })
                    {
                        return Err(CheckpointSetupError::Corrupt(
                            "duplicate or malformed estat",
                        ));
                    }
                    extended_statistics.push(LoadedExtendedStatistics {
                        database: storage.current_database_oid(),
                        created_at,
                        table_index,
                        schema,
                        name,
                        target,
                        kinds,
                        expression_only,
                        keys,
                        n_keys: n_keys as u8,
                        data: crate::storage::ExtendedStatisticsData::EMPTY,
                    });
                }
                Some("estatdata") => {
                    let created_at: u64 = parse_field(words.next(), "estatdata identity")?;
                    let statistics = extended_statistics
                        .iter_mut()
                        .find(|statistics| {
                            statistics.database == storage.current_database_oid()
                                && statistics.created_at == created_at
                        })
                        .ok_or(CheckpointSetupError::Corrupt("estatdata precedes estat"))?;
                    if statistics.data.valid {
                        return Err(CheckpointSetupError::Corrupt("duplicate estatdata"));
                    }
                    statistics.data.valid = true;
                    statistics.data.inherited =
                        match parse_field::<u8>(words.next(), "estatdata inherited")? {
                            0 => false,
                            1 => true,
                            _ => {
                                return Err(CheckpointSetupError::Corrupt(
                                    "invalid estatdata inherited",
                                ));
                            }
                        };
                    statistics.data.analyzed_generation =
                        parse_field(words.next(), "estatdata generation")?;
                    statistics.data.rows = parse_field(words.next(), "estatdata rows")?;
                    statistics.data.non_null_rows = parse_field(words.next(), "estatdata nonnull")?;
                    statistics.data.distinct_values =
                        parse_field(words.next(), "estatdata distinct")?;
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt("malformed estatdata"));
                    }
                }
                Some("estatdep") => {
                    let created_at: u64 = parse_field(words.next(), "estatdep identity")?;
                    let index: usize = parse_field(words.next(), "estatdep index")?;
                    let strength: u32 = parse_field(words.next(), "estatdep strength")?;
                    let statistics = extended_statistics
                        .iter_mut()
                        .find(|statistics| {
                            statistics.database == storage.current_database_oid()
                                && statistics.created_at == created_at
                        })
                        .ok_or(CheckpointSetupError::Corrupt("estatdep precedes estat"))?;
                    if index >= statistics.data.dependencies_ppm.len()
                        || strength > 1_000_000
                        || statistics.data.dependencies_ppm[index] != 0
                        || words.next().is_some()
                    {
                        return Err(CheckpointSetupError::Corrupt("invalid estatdep"));
                    }
                    statistics.data.dependencies_ppm[index] = strength;
                }
                Some("estatexpr") => {
                    let created_at: u64 = parse_field(words.next(), "estatexpr identity")?;
                    let key: usize = parse_field(words.next(), "estatexpr key")?;
                    let statistics = extended_statistics
                        .iter_mut()
                        .find(|statistics| {
                            statistics.database == storage.current_database_oid()
                                && statistics.created_at == created_at
                        })
                        .ok_or(CheckpointSetupError::Corrupt("estatexpr precedes estat"))?;
                    if key >= usize::from(statistics.n_keys)
                        || statistics.data.expression_statistics[key].valid
                    {
                        return Err(CheckpointSetupError::Corrupt("invalid estatexpr key"));
                    }
                    let null_fraction_ppm = parse_field(words.next(), "estatexpr null fraction")?;
                    let distinct_values = parse_field(words.next(), "estatexpr distinct")?;
                    let distinct_fraction_ppm =
                        parse_field(words.next(), "estatexpr distinct fraction")?;
                    let average_width = parse_field(words.next(), "estatexpr width")?;
                    if null_fraction_ppm > 1_000_000
                        || distinct_fraction_ppm > 1_000_000
                        || words.next().is_some()
                    {
                        return Err(CheckpointSetupError::Corrupt("invalid estatexpr"));
                    }
                    statistics.data.expression_statistics[key] = crate::storage::ColumnStatistics {
                        valid: true,
                        null_fraction_ppm,
                        distinct_values,
                        distinct_fraction_ppm,
                        average_width,
                    };
                }
                Some("estatmcv") => {
                    let created_at: u64 = parse_field(words.next(), "estatmcv identity")?;
                    let hash: u64 = parse_field(words.next(), "estatmcv hash")?;
                    let count: u64 = parse_field(words.next(), "estatmcv count")?;
                    let encoded = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("estatmcv value missing"))?;
                    let statistics = extended_statistics
                        .iter_mut()
                        .find(|statistics| {
                            statistics.database == storage.current_database_oid()
                                && statistics.created_at == created_at
                        })
                        .ok_or(CheckpointSetupError::Corrupt("estatmcv precedes estat"))?;
                    let position = usize::from(statistics.data.n_mcv);
                    if position >= crate::storage::MAX_EXTENDED_STATISTICS_MCV
                        || words.next().is_some()
                    {
                        return Err(CheckpointSetupError::Corrupt(
                            "too many or malformed estatmcv",
                        ));
                    }
                    let decoded = decode_hex_name(encoded)?;
                    let values = crate::util::StackStr::from_str(&decoded);
                    if values.is_truncated() {
                        return Err(CheckpointSetupError::Corrupt("estatmcv value is too long"));
                    }
                    statistics.data.mcv[position] = crate::storage::ExtendedStatisticsMcv {
                        valid: true,
                        hash,
                        count,
                        base_frequency_bits: 0,
                        values,
                    };
                    statistics.data.n_mcv += 1;
                }
                Some("estatmcvbase") => {
                    let created_at: u64 = parse_field(words.next(), "estatmcvbase identity")?;
                    let hash: u64 = parse_field(words.next(), "estatmcvbase hash")?;
                    let bits: u64 = parse_field(words.next(), "estatmcvbase bits")?;
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt("invalid estatmcvbase"));
                    }
                    let statistics = extended_statistics
                        .iter_mut()
                        .find(|statistics| {
                            statistics.database == storage.current_database_oid()
                                && statistics.created_at == created_at
                        })
                        .ok_or(CheckpointSetupError::Corrupt("estatmcvbase precedes estat"))?;
                    let entry = statistics.data.mcv[..usize::from(statistics.data.n_mcv)]
                        .iter_mut()
                        .find(|entry| entry.hash == hash)
                        .ok_or(CheckpointSetupError::Corrupt(
                            "estatmcvbase precedes estatmcv",
                        ))?;
                    let base_frequency = f64::from_bits(bits);
                    if !base_frequency.is_finite() || !(0.0..=1.0).contains(&base_frequency) {
                        return Err(CheckpointSetupError::Corrupt("invalid estatmcvbase bits"));
                    }
                    entry.base_frequency_bits = bits;
                }
                Some("tsch") => {
                    let Some((_, _, def, _, _)) = pending_def.as_mut() else {
                        return Err(CheckpointSetupError::Corrupt("tsch outside table"));
                    };
                    let hex = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("tsch name missing"))?;
                    def.schema = sql_name(&decode_hex_name(hex)?)?;
                }
                Some("part") => {
                    let Some((_, _, def, _, _)) = pending_def.as_mut() else {
                        return Err(CheckpointSetupError::Corrupt("part outside table"));
                    };
                    def.partition = parse_partition_manifest(&mut words)?;
                }
                Some("rls") => {
                    let Some((_, _, def, _, _)) = pending_def.as_mut() else {
                        return Err(CheckpointSetupError::Corrupt("rls outside table"));
                    };
                    def.row_level_security = crate::storage::RowLevelSecurityState {
                        enabled: match parse_field::<u8>(words.next(), "rls enabled")? {
                            0 => false,
                            1 => true,
                            _ => return Err(CheckpointSetupError::Corrupt("rls enabled")),
                        },
                        forced: match parse_field::<u8>(words.next(), "rls forced")? {
                            0 => false,
                            1 => true,
                            _ => return Err(CheckpointSetupError::Corrupt("rls forced")),
                        },
                    };
                    def.replica_identity = crate::storage::ReplicaIdentityMode::from_code(
                        parse_field(words.next(), "replica identity mode")?,
                    )
                    .ok_or(CheckpointSetupError::Corrupt("replica identity mode"))?;
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt("trailing rls fields"));
                    }
                }
                Some("nsp") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let created_at = parse_field::<u64>(words.next(), "schema creation identity")?;
                    if created_at > metadata.catalog_sequence {
                        return Err(CheckpointSetupError::Corrupt(
                            "schema creation identity exceeds its allocator",
                        ));
                    }
                    let identity = crate::storage::SchemaIdentity::parse(created_at).ok_or(
                        CheckpointSetupError::Corrupt("invalid schema creation identity"),
                    )?;
                    let database_owner =
                        match parse_field::<u8>(words.next(), "schema database owner")? {
                            0 => false,
                            1 => true,
                            _ => {
                                return Err(CheckpointSetupError::Corrupt(
                                    "invalid schema database owner",
                                ));
                            }
                        };
                    let hex = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("nsp name missing"))?;
                    let name = sql_name(&decode_hex_name(hex)?)?;
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt("trailing schema fields"));
                    }
                    storage
                        .restore_schema(
                            identity,
                            name,
                            crate::storage::CatalogDdlState::Present,
                            database_owner,
                        )
                        .map_err(|e| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest schema rejected: {}",
                                e.message.as_str()
                            ))
                        })?;
                }
                Some("lob") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let oid = parse_field::<u32>(words.next(), "large-object OID")?;
                    let created_at = parse_field::<u64>(words.next(), "large-object creation")?;
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt(
                            "trailing large-object fields",
                        ));
                    }
                    let oid = crate::sql::ast::LargeObjectId::parse(oid)
                        .ok_or(CheckpointSetupError::Corrupt("invalid large-object OID"))?;
                    storage
                        .restore_large_object(oid, created_at, false)
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest large object rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("rol") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let oid = crate::storage::RoleOid::parse(parse_field(words.next(), "rol OID")?)
                        .filter(|oid| oid.get() as u64 <= metadata.role_sequence)
                        .ok_or(CheckpointSetupError::Corrupt("invalid rol OID"))?;
                    let slot: usize = parse_field(words.next(), "rol slot")?;
                    let name = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("rol name missing"))?,
                    )?;
                    let flags: u16 = parse_field(words.next(), "rol flags")?;
                    let connection_limit: i32 = parse_field(words.next(), "rol connection limit")?;
                    let credential_kind = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("rol credential kind missing"))?;
                    let credential_value = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("rol credential missing"))?;
                    let stored_key_encoded = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("rol stored key missing"))?;
                    let server_key_encoded = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("rol server key missing"))?;
                    let iterations: u32 = parse_field(words.next(), "rol iterations")?;
                    let valid_until_encoded = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("rol valid-until missing"))?;
                    let password_present = flags & (1 << 7) != 0;
                    let valid_until_present = flags & (1 << 8) != 0;
                    let password = match credential_kind {
                        "-" if credential_value == "-"
                            && stored_key_encoded == "-"
                            && server_key_encoded == "-"
                            && iterations == 0 =>
                        {
                            None
                        }
                        "s" => {
                            let salt = parse_hex_salt(credential_value)?;
                            let stored_key = parse_hex_array::<32>(stored_key_encoded)?;
                            let server_key = parse_hex_array::<32>(server_key_encoded)?;
                            if iterations == 0 {
                                return Err(CheckpointSetupError::Corrupt(
                                    "invalid SCRAM verifier",
                                ));
                            }
                            Some(crate::storage::RoleCredential::Scram(
                                crate::pg::auth::ScramServer {
                                    salt,
                                    stored_key,
                                    server_key,
                                    iterations,
                                },
                            ))
                        }
                        "m" if stored_key_encoded == "-"
                            && server_key_encoded == "-"
                            && iterations == 0 =>
                        {
                            let bytes = credential_value.as_bytes();
                            if bytes.len() != 32 || !bytes.iter().all(u8::is_ascii_hexdigit) {
                                return Err(CheckpointSetupError::Corrupt("invalid MD5 verifier"));
                            }
                            let mut hash = [0; 32];
                            hash.copy_from_slice(bytes);
                            Some(crate::storage::RoleCredential::Md5(
                                crate::storage::Md5Verifier { hash },
                            ))
                        }
                        _ => return Err(CheckpointSetupError::Corrupt("invalid rol credential")),
                    };
                    let valid_until = match (valid_until_present, valid_until_encoded) {
                        (false, "-") => None,
                        (true, "0") => Some(crate::util::StackStr::new()),
                        (true, encoded) => {
                            let decoded = decode_hex_name(encoded)?;
                            if decoded.len() > crate::storage::ROLE_VALID_UNTIL_MAX {
                                return Err(CheckpointSetupError::Corrupt("invalid rol record"));
                            }
                            Some(crate::util::StackStr::from_str(&decoded))
                        }
                        (false, _) => {
                            return Err(CheckpointSetupError::Corrupt("invalid rol record"));
                        }
                    };
                    if words.next().is_some()
                        || flags & !0x01ff != 0
                        || (password_present != password.is_some())
                    {
                        return Err(CheckpointSetupError::Corrupt("invalid rol record"));
                    }
                    storage
                        .install_role(
                            sql_name(&name)?,
                            crate::storage::RoleAttributes {
                                superuser: flags & 1 != 0,
                                inherit: flags & (1 << 1) != 0,
                                create_role: flags & (1 << 2) != 0,
                                create_database: flags & (1 << 3) != 0,
                                can_login: flags & (1 << 4) != 0,
                                replication: flags & (1 << 5) != 0,
                                bypass_row_level_security: flags & (1 << 6) != 0,
                                connection_limit,
                                password,
                                valid_until,
                            },
                            oid,
                            slot,
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest role rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("rmem") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let identity = parse_catalog_position(&mut words, metadata.catalog_sequence)?;
                    let role_oid =
                        |word, name| -> Result<crate::storage::RoleOid, CheckpointSetupError> {
                            crate::storage::RoleOid::parse(parse_field(word, name)?).ok_or(
                                CheckpointSetupError::Corrupt("invalid membership role identifier"),
                            )
                        };
                    let role = role_oid(words.next(), "rmem role missing")?;
                    let member = role_oid(words.next(), "rmem member missing")?;
                    let grantor = role_oid(words.next(), "rmem grantor missing")?;
                    let flags: u8 = parse_field(words.next(), "rmem flags")?;
                    if words.next().is_some() || flags & !0x07 != 0 {
                        return Err(CheckpointSetupError::Corrupt("invalid rmem record"));
                    }
                    storage
                        .install_role_membership(
                            identity,
                            role,
                            member,
                            grantor,
                            crate::storage::RoleMembershipOptions {
                                admin: flags & 1 != 0,
                                inherit: flags & 2 != 0,
                                set: flags & 4 != 0,
                            },
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest role membership rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("db") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let oid: i32 = parse_field(words.next(), "db oid")?;
                    let oid = crate::storage::DatabaseOid::parse(oid)
                        .ok_or(CheckpointSetupError::Corrupt("invalid db oid"))?;
                    let name = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("db name missing"))?,
                    )?;
                    let owner_name = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("db owner missing"))?,
                    )?;
                    let flags: u8 = parse_field(words.next(), "db flags")?;
                    let encoding: u8 = parse_field(words.next(), "db encoding")?;
                    let provider: u8 = parse_field(words.next(), "db locale provider")?;
                    let tablespace: u16 = parse_field(words.next(), "db tablespace")?;
                    let connection_limit: i32 = parse_field(words.next(), "db connection limit")?;
                    let collate = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("db collate missing"))?,
                    )?;
                    let ctype = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("db ctype missing"))?,
                    )?;
                    let locale = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("db locale missing"))?,
                    )?;
                    let collation_version = decode_hex_name(words.next().ok_or(
                        CheckpointSetupError::Corrupt("db collation version missing"),
                    )?)?;
                    if words.next().is_some() || flags & !3 != 0 || connection_limit < -1 {
                        return Err(CheckpointSetupError::Corrupt("invalid db record"));
                    }
                    let owner = storage
                        .find_role(&owner_name)
                        .ok_or(CheckpointSetupError::Corrupt("db owner does not exist"))?;
                    let definition = crate::storage::DatabaseDefinition {
                        name: sql_name(&name)?,
                        encoding: crate::storage::DatabaseEncoding::from_code(encoding)
                            .ok_or(CheckpointSetupError::Corrupt("invalid db encoding"))?,
                        locale_provider: crate::storage::DatabaseLocaleProvider::from_code(
                            provider,
                        )
                        .ok_or(CheckpointSetupError::Corrupt("invalid db locale provider"))?,
                        collate: crate::util::StackStr::from_str(&collate),
                        ctype: crate::util::StackStr::from_str(&ctype),
                        locale: crate::util::StackStr::from_str(&locale),
                        collation_version: crate::util::StackStr::from_str(&collation_version),
                        allow_connections: flags & 1 != 0,
                        is_template: flags & 2 != 0,
                        connection_limit,
                        tablespace,
                    };
                    if definition.collate.is_truncated()
                        || definition.ctype.is_truncated()
                        || definition.locale.is_truncated()
                        || definition.collation_version.is_truncated()
                    {
                        return Err(CheckpointSetupError::Corrupt("db field is too long"));
                    }
                    if database_identities.iter().any(|&(seen_oid, seen_name)| {
                        seen_oid == oid || seen_name == definition.name
                    }) {
                        return Err(CheckpointSetupError::Corrupt("duplicate database record"));
                    }
                    if database_identities.len() == storage.database_count() {
                        return Err(CheckpointSetupError::Corrupt("too many database records"));
                    }
                    database_identities.push((oid, definition.name));
                    if let Some(slot) = storage.database_slot_by_oid(oid, 0) {
                        storage
                            .alter_database_definition(slot, definition, 0)
                            .map_err(|_| CheckpointSetupError::Corrupt("invalid built-in db"))?;
                        storage.set_object_owner(
                            crate::storage::AccessObject {
                                class: crate::storage::AccessClass::Database,
                                slot: slot as u16,
                            },
                            owner,
                            0,
                        );
                        storage.commit_database_alter(slot, 0);
                    } else {
                        storage
                            .restore_database(oid, definition, owner as u16)
                            .map_err(|error| {
                                CheckpointSetupError::ObjectStore(format!(
                                    "manifest database rejected: {}",
                                    error.message.as_str()
                                ))
                            })?;
                    }
                }
                Some("rset") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let scope: u8 = parse_field(words.next(), "rset scope")?;
                    let role = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("rset role missing"))?;
                    let database = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("rset database missing"))?;
                    let name = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("rset name missing"))?,
                    )?;
                    let encoded_value = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("rset value missing"))?;
                    let value = if encoded_value == "0" {
                        String::new()
                    } else {
                        decode_hex_name(encoded_value)?
                    };
                    if words.next().is_some()
                        || value.len() > crate::storage::ROLE_SETTING_VALUE_MAX
                    {
                        return Err(CheckpointSetupError::Corrupt("invalid rset record"));
                    }
                    let database_oid = if database == "-" {
                        None
                    } else {
                        let oid: i32 = database
                            .parse()
                            .map_err(|_| CheckpointSetupError::Corrupt("invalid rset database"))?;
                        let oid = crate::storage::DatabaseOid::parse(oid)
                            .ok_or(CheckpointSetupError::Corrupt("invalid rset database"))?;
                        storage.database_slot_by_oid(oid, 0).ok_or(
                            CheckpointSetupError::Corrupt("rset database does not exist"),
                        )?;
                        Some(oid)
                    };
                    let scope = match scope {
                        0 | 1 => {
                            let role = decode_hex_name(role)?;
                            let slot = storage
                                .find_role(&role)
                                .ok_or(CheckpointSetupError::Corrupt("rset role does not exist"))?
                                as u16;
                            if scope == 0 {
                                if database_oid.is_some() {
                                    return Err(CheckpointSetupError::Corrupt(
                                        "invalid global rset database",
                                    ));
                                }
                                crate::storage::RoleSettingScope::RoleAllDatabases(slot)
                            } else {
                                crate::storage::RoleSettingScope::RoleInDatabase {
                                    role: slot,
                                    database: database_oid.ok_or(CheckpointSetupError::Corrupt(
                                        "rset database missing",
                                    ))?,
                                }
                            }
                        }
                        2 if role == "-" => crate::storage::RoleSettingScope::AllRolesInDatabase(
                            database_oid
                                .ok_or(CheckpointSetupError::Corrupt("rset database missing"))?,
                        ),
                        _ => return Err(CheckpointSetupError::Corrupt("invalid rset scope")),
                    };
                    storage
                        .install_role_setting(
                            scope,
                            sql_name(&name)?,
                            Some(crate::util::StackStr::from_str(&value)),
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest role setting rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("sset") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let name = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("sset name missing"))?,
                    )?;
                    let value = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("sset value missing"))?,
                    )?;
                    if words.next().is_some()
                        || value.len() > crate::storage::ROLE_SETTING_VALUE_MAX
                    {
                        return Err(CheckpointSetupError::Corrupt("invalid sset record"));
                    }
                    let name = sql_name(&name)?;
                    if system_setting_names.contains(&name) {
                        return Err(CheckpointSetupError::Corrupt(
                            "duplicate system setting record",
                        ));
                    }
                    if system_setting_names.len() == crate::storage::MAX_SYSTEM_SETTINGS {
                        return Err(CheckpointSetupError::Corrupt(
                            "too many system setting records",
                        ));
                    }
                    system_setting_names.push(name);
                    storage
                        .install_system_setting(name, Some(crate::util::StackStr::from_str(&value)))
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest system setting rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("ptx") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let transaction_id = parse_field(words.next(), "ptx transaction")?;
                    let first_lsn = parse_field(words.next(), "ptx first lsn")?;
                    let prepared_lsn = parse_field(words.next(), "ptx prepared lsn")?;
                    let prepared_at = parse_field(words.next(), "ptx prepared time")?;
                    let database = parse_field::<i32>(words.next(), "ptx database")?;
                    let database = crate::storage::DatabaseOid::parse(database)
                        .ok_or(CheckpointSetupError::Corrupt("invalid ptx database"))?;
                    if storage.database_slot_by_oid(database, 0).is_none() {
                        return Err(CheckpointSetupError::Corrupt("ptx database does not exist"));
                    }
                    let owner =
                        crate::storage::RoleOid::parse(parse_field(words.next(), "ptx owner")?)
                            .filter(|oid| oid.get() as u64 <= metadata.role_sequence)
                            .ok_or(CheckpointSetupError::Corrupt("invalid ptx owner"))?;
                    let gid = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("ptx gid missing"))?,
                    )?;
                    if words.next().is_some()
                        || first_lsn == 0
                        || prepared_lsn < first_lsn
                        || gid.len() > 199
                    {
                        return Err(CheckpointSetupError::Corrupt("invalid ptx record"));
                    }
                    storage
                        .install_prepared_transaction_catalog_entry(
                            crate::storage::PreparedTransactionCatalogEntry {
                                transaction_id,
                                gid: crate::util::StackStr::from_str(&gid),
                                prepared_at,
                                owner,
                                database,
                                first_lsn,
                                prepared_lsn,
                            },
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest prepared transaction rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("own") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let class: u8 = parse_field(words.next(), "own class")?;
                    let class = crate::storage::AccessClass::from_u8(class)
                        .ok_or(CheckpointSetupError::Corrupt("invalid own class"))?;
                    let object_oid = if class == crate::storage::AccessClass::Routine {
                        parse_field(words.next(), "own routine oid")?
                    } else {
                        0
                    };
                    let schema_word = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("own schema missing"))?;
                    let schema = if schema_word == "-" {
                        String::new()
                    } else {
                        decode_hex_name(schema_word)?
                    };
                    let name = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("own name missing"))?,
                    )?;
                    let owner = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("own owner missing"))?,
                    )?;
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt("malformed own record"));
                    }
                    let object = (if class == crate::storage::AccessClass::Routine {
                        storage
                            .routine_slot_by_oid(object_oid, 0)
                            .map(crate::storage::Storage::routine_access_object)
                    } else {
                        storage.resolve_access_object(class, &schema, &name, 0)
                    })
                    .ok_or_else(|| {
                        CheckpointSetupError::ObjectStore(format!(
                            "manifest ownership target does not exist: database {} class {} schema {:?} name {:?}",
                            storage.current_database_oid().get(),
                            class as u8,
                            schema,
                            name,
                        ))
                    })?;
                    let owner = storage
                        .find_role(&owner)
                        .ok_or(CheckpointSetupError::Corrupt("own role does not exist"))?;
                    if object.class == crate::storage::AccessClass::Tablespace {
                        storage
                            .set_tablespace_owner(usize::from(object.slot), owner, 0)
                            .map_err(|error| {
                                CheckpointSetupError::ObjectStore(format!(
                                    "manifest tablespace ownership rejected: {}",
                                    error.message.as_str()
                                ))
                            })?;
                    } else if object.class == crate::storage::AccessClass::Schema {
                        storage
                            .set_schema_owner(usize::from(object.slot), owner, 0)
                            .map_err(|error| {
                                CheckpointSetupError::ObjectStore(format!(
                                    "manifest schema ownership rejected: {}",
                                    error.message.as_str()
                                ))
                            })?;
                    } else if object.class == crate::storage::AccessClass::Extension {
                        storage
                            .set_extension_owner(usize::from(object.slot), owner, 0)
                            .map_err(|error| {
                                CheckpointSetupError::ObjectStore(format!(
                                    "manifest extension ownership rejected: {}",
                                    error.message.as_str()
                                ))
                            })?;
                    } else {
                        storage.set_object_owner(object, owner, 0);
                    }
                }
                Some("acl") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let class: u8 = parse_field(words.next(), "acl class")?;
                    let class = crate::storage::AccessClass::from_u8(class)
                        .ok_or(CheckpointSetupError::Corrupt("invalid acl class"))?;
                    let object_oid = if class == crate::storage::AccessClass::Routine {
                        parse_field(words.next(), "acl routine oid")?
                    } else {
                        0
                    };
                    let decode = |word: Option<&str>, missing: &'static str| {
                        word.ok_or(CheckpointSetupError::Corrupt(missing))
                            .and_then(decode_hex_name)
                    };
                    let schema = decode(words.next(), "acl schema missing")?;
                    let name = decode(words.next(), "acl name missing")?;
                    let grantee = decode(words.next(), "acl grantee missing")?;
                    let grantor = decode(words.next(), "acl grantor missing")?;
                    let privileges: u16 = parse_field(words.next(), "acl privileges")?;
                    let grant_options: u16 = parse_field(words.next(), "acl grant options")?;
                    let order =
                        core::num::NonZeroU64::new(parse_field(words.next(), "acl grant order")?)
                            .ok_or(CheckpointSetupError::Corrupt("invalid acl grant order"))?;
                    if words.next().is_some()
                        || privileges & !crate::storage::all_object_privileges(class).0 != 0
                        || grant_options & !privileges != 0
                    {
                        return Err(CheckpointSetupError::Corrupt("invalid acl record"));
                    }
                    let object = (if class == crate::storage::AccessClass::Routine {
                        storage
                            .routine_slot_by_oid(object_oid, 0)
                            .map(crate::storage::Storage::routine_access_object)
                    } else {
                        storage.resolve_access_object(class, &schema, &name, 0)
                    })
                    .ok_or(CheckpointSetupError::Corrupt("acl target does not exist"))?;
                    let grantee = if grantee == "PUBLIC" {
                        crate::storage::PUBLIC_ROLE
                    } else {
                        storage
                            .find_role(&grantee)
                            .ok_or(CheckpointSetupError::Corrupt("acl grantee does not exist"))?
                            as u16
                    };
                    let grantor = storage
                        .find_role(&grantor)
                        .ok_or(CheckpointSetupError::Corrupt("acl grantor does not exist"))?
                        as u16;
                    storage
                        .change_acl(
                            object,
                            grantee,
                            grantor,
                            crate::storage::PrivilegeSet(privileges),
                            crate::storage::PrivilegeSet(grant_options),
                            crate::storage::ObjectAclWrite::Exact {
                                transaction: 0,
                                order,
                            },
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest ACL rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("cacl") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let class: u8 = parse_field(words.next(), "cacl class")?;
                    let class = crate::storage::AccessClass::from_u8(class)
                        .filter(|class| {
                            matches!(
                                class,
                                crate::storage::AccessClass::Table
                                    | crate::storage::AccessClass::View
                                    | crate::storage::AccessClass::MaterializedView
                            )
                        })
                        .ok_or(CheckpointSetupError::Corrupt("invalid cacl class"))?;
                    let decode = |word: Option<&str>, missing: &'static str| {
                        word.ok_or(CheckpointSetupError::Corrupt(missing))
                            .and_then(decode_hex_name)
                    };
                    let schema = decode(words.next(), "cacl schema missing")?;
                    let name = decode(words.next(), "cacl name missing")?;
                    let column: u16 = parse_field(words.next(), "cacl column")?;
                    let grantee = decode(words.next(), "cacl grantee missing")?;
                    let grantor = decode(words.next(), "cacl grantor missing")?;
                    let privileges: u16 = parse_field(words.next(), "cacl privileges")?;
                    let grant_options: u16 = parse_field(words.next(), "cacl grant options")?;
                    let allowed = (crate::storage::PrivilegeSet::SELECT
                        .union(crate::storage::PrivilegeSet::INSERT)
                        .union(crate::storage::PrivilegeSet::UPDATE)
                        .union(crate::storage::PrivilegeSet::REFERENCES))
                    .0;
                    if words.next().is_some()
                        || privileges & !allowed != 0
                        || grant_options & !privileges != 0
                    {
                        return Err(CheckpointSetupError::Corrupt("invalid cacl record"));
                    }
                    let relation = storage
                        .resolve_access_object(class, &schema, &name, 0)
                        .ok_or(CheckpointSetupError::Corrupt("cacl target does not exist"))?;
                    let column_count = match class {
                        crate::storage::AccessClass::Table => {
                            Some(storage.table_def(relation.slot as usize, 0).n_columns)
                        }
                        crate::storage::AccessClass::MaterializedView => storage
                            .find_table(&schema, &name)
                            .map(|table| storage.table_def(table, 0).n_columns),
                        crate::storage::AccessClass::View => Some(crate::sql::exec::MAX_PROJ),
                        _ => unreachable!("cacl class was restricted above"),
                    };
                    if column_count.is_some_and(|count| column as usize >= count) {
                        return Err(CheckpointSetupError::Corrupt("cacl column does not exist"));
                    }
                    let target = crate::storage::ColumnPrivilegeTarget::new(relation, column)
                        .map_err(|_| CheckpointSetupError::Corrupt("invalid cacl target"))?;
                    let grantee = if grantee == "PUBLIC" {
                        crate::storage::PUBLIC_ROLE
                    } else {
                        storage
                            .find_role(&grantee)
                            .ok_or(CheckpointSetupError::Corrupt("cacl grantee does not exist"))?
                            as u16
                    };
                    let grantor = storage
                        .find_role(&grantor)
                        .ok_or(CheckpointSetupError::Corrupt("cacl grantor does not exist"))?
                        as u16;
                    storage
                        .change_column_acl(
                            target,
                            grantee,
                            grantor,
                            crate::storage::PrivilegeSet(privileges),
                            crate::storage::PrivilegeSet(grant_options),
                            0,
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest column ACL rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("dacl") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let slot: u16 = parse_field(words.next(), "default privilege position")?;
                    let creation: u64 = parse_field(words.next(), "default privilege creation")?;
                    let creation = crate::storage::CatalogIdentity::parse(creation)
                        .filter(|creation| creation.created_at() <= metadata.catalog_sequence)
                        .ok_or(CheckpointSetupError::Corrupt(
                            "invalid default privilege creation",
                        ))?;
                    let owner: i32 = parse_field(words.next(), "default privilege owner")?;
                    let owner = crate::storage::RoleOid::parse(owner).ok_or(
                        CheckpointSetupError::Corrupt("invalid default privilege owner"),
                    )?;
                    let namespace = words.next().ok_or(CheckpointSetupError::Corrupt(
                        "default privilege namespace missing",
                    ))?;
                    let schema = if namespace == "-" {
                        None
                    } else {
                        let created_at: u64 =
                            parse_field(Some(namespace), "default privilege namespace")?;
                        Some(crate::storage::SchemaIdentity::parse(created_at).ok_or(
                            CheckpointSetupError::Corrupt("invalid default privilege namespace"),
                        )?)
                    };
                    let grantee = words.next().ok_or(CheckpointSetupError::Corrupt(
                        "default privilege grantee missing",
                    ))?;
                    let grantee = if grantee == "PUBLIC" {
                        None
                    } else {
                        let oid: i32 = parse_field(Some(grantee), "default privilege grantee")?;
                        Some(crate::storage::RoleOid::parse(oid).ok_or(
                            CheckpointSetupError::Corrupt("invalid default privilege grantee"),
                        )?)
                    };
                    let class: u8 = parse_field(words.next(), "default privilege class")?;
                    let class = crate::storage::DefaultPrivilegeClass::from_u8(class).ok_or(
                        CheckpointSetupError::Corrupt("invalid default privilege class"),
                    )?;
                    let privileges: u16 = parse_field(words.next(), "default privileges")?;
                    let grant_options: u16 = parse_field(words.next(), "default grant options")?;
                    if words.next().is_some()
                        || privileges & !class.all_privileges().0 != 0
                        || grant_options & !privileges != 0
                        || slot == u16::MAX
                    {
                        return Err(CheckpointSetupError::Corrupt(
                            "invalid default privilege record",
                        ));
                    }
                    storage
                        .install_default_acl_image(
                            usize::from(slot),
                            None,
                            Some(crate::storage::DefaultAclGrantImage {
                                identity: crate::storage::DefaultAclIdentity {
                                    creation,
                                    owner,
                                    schema,
                                    class,
                                },
                                grantee,
                                privileges: crate::storage::PrivilegeSet(privileges),
                                grant_options: crate::storage::PrivilegeSet(grant_options),
                            }),
                            0,
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest default privilege rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("pacl") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let oid: u32 = parse_field(words.next(), "pacl oid")?;
                    let oid = crate::storage::ParameterAclOid::parse(oid)
                        .ok_or(CheckpointSetupError::Corrupt("invalid pacl oid"))?;
                    let parameter = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("pacl parameter missing"))?,
                    )?;
                    let parameter = crate::sql::ast::ParameterName::parse(&parameter)
                        .ok_or(CheckpointSetupError::Corrupt("invalid pacl parameter"))?;
                    let bytes = decode_manifest_hex_bytes(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("pacl grants missing"))?,
                    )?;
                    let grants = crate::storage::ParameterAclGrants::parse(&bytes)
                        .ok_or(CheckpointSetupError::Corrupt("invalid pacl grants"))?;
                    let image = crate::storage::ParameterAclImage::new(oid, grants).ok_or(
                        CheckpointSetupError::Corrupt("pacl row contains implicit defaults"),
                    )?;
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt("invalid pacl record"));
                    }
                    storage
                        .restore_parameter_acl_row(parameter, None, Some(image))
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest parameter ACL rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("seq") => {
                    let Some((_, _, _, _, serials)) = pending_def.as_mut() else {
                        return Err(CheckpointSetupError::Corrupt("seq outside table"));
                    };
                    let column: usize = parse_field(words.next(), "seq column")?;
                    let last: i64 = parse_field(words.next(), "seq last")?;
                    if column >= crate::storage::MAX_COLUMNS {
                        return Err(CheckpointSetupError::Corrupt("seq column out of range"));
                    }
                    serials[column] = last;
                }
                Some("ukey") => {
                    let Some((_, _, def, _, _)) = pending_def.as_mut() else {
                        return Err(CheckpointSetupError::Corrupt("ukey outside table"));
                    };
                    if def.n_uniques >= crate::storage::MAX_UNIQUES {
                        return Err(CheckpointSetupError::Corrupt("too many ukey lines"));
                    }
                    let kind: u8 = parse_field(words.next(), "ukey kind")?;
                    let timing: u8 = parse_field(words.next(), "ukey timing")?;
                    let n_cols: usize = parse_field(words.next(), "ukey ncols")?;
                    if n_cols == 0 || n_cols > crate::storage::MAX_INDEX_COLS {
                        return Err(CheckpointSetupError::Corrupt("bad ukey ncols"));
                    }
                    let mut uk = crate::storage::UniqueKey::EMPTY;
                    uk.kind = crate::storage::UniqueKeyKind::from_code(kind)
                        .ok_or(CheckpointSetupError::Corrupt("bad ukey kind"))?;
                    uk.timing = crate::storage::ConstraintTiming::from_code(timing)
                        .ok_or(CheckpointSetupError::Corrupt("bad ukey timing"))?;
                    uk.n_cols = n_cols;
                    for c in uk.columns.iter_mut().take(n_cols) {
                        *c = parse_field(words.next(), "ukey col")?;
                    }
                    for (position, column) in uk.columns[..n_cols].iter().enumerate() {
                        if usize::from(*column) >= def.n_columns
                            || uk.columns[..position].contains(column)
                        {
                            return Err(CheckpointSetupError::Corrupt("bad ukey column"));
                        }
                    }
                    let hex_name = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("ukey name missing"))?;
                    uk.name = sql_name(&decode_hex_name(hex_name)?)?;
                    let i = def.n_uniques;
                    def.uniques[i] = uk;
                    def.n_uniques += 1;
                }
                Some("chk") => {
                    let Some((_, _, def, _, _)) = pending_def.as_mut() else {
                        return Err(CheckpointSetupError::Corrupt("chk outside table"));
                    };
                    if def.n_checks >= crate::storage::MAX_CHECKS {
                        return Err(CheckpointSetupError::Corrupt("too many chk lines"));
                    }
                    let validation: u8 = parse_field(words.next(), "chk validation")?;
                    let hex_name = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("chk name missing"))?;
                    let hexpr = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("chk expression missing"))?;
                    let mut check = crate::storage::CheckConstraint::EMPTY;
                    check.validation = crate::storage::ConstraintValidation::from_code(validation)
                        .ok_or(CheckpointSetupError::Corrupt("bad chk validation"))?;
                    check.name = sql_name(&decode_hex_name(hex_name)?)?;
                    let expression = decode_hex_name(hexpr)?;
                    use core::fmt::Write;
                    let _ = write!(check.expression, "{expression}");
                    if check.expression.is_truncated() {
                        return Err(CheckpointSetupError::Corrupt("chk predicate too long"));
                    }
                    let i = def.n_checks;
                    def.checks[i] = check;
                    def.n_checks += 1;
                }
                Some("fkey") => {
                    let Some((_, _, def, _, _)) = pending_def.as_mut() else {
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
                    for c in fk.columns.iter_mut().take(n_cols) {
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
                    let timing: u8 = parse_field(words.next(), "fkey timing")?;
                    let validation: u8 = parse_field(words.next(), "fkey validation")?;
                    fk.on_delete = crate::storage::FkAction::from_code(od)
                        .ok_or(CheckpointSetupError::Corrupt("bad fkey on_delete"))?;
                    fk.on_update = crate::storage::FkAction::from_code(ou)
                        .ok_or(CheckpointSetupError::Corrupt("bad fkey on_update"))?;
                    fk.timing = crate::storage::ConstraintTiming::from_code(timing)
                        .ok_or(CheckpointSetupError::Corrupt("bad fkey timing"))?;
                    fk.validation = crate::storage::ConstraintValidation::from_code(validation)
                        .ok_or(CheckpointSetupError::Corrupt("bad fkey validation"))?;
                    let hex_name = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("fkey name missing"))?;
                    let hparent = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("fkey parent missing"))?;
                    fk.name = sql_name(&decode_hex_name(hex_name)?)?;
                    fk.parent = sql_name(&decode_hex_name(hparent)?)?;
                    fk.parent_schema =
                        sql_name(&decode_hex_name(words.next().ok_or(
                            CheckpointSetupError::Corrupt("fkey parent schema missing"),
                        )?)?)?;
                    let i = def.n_fkeys;
                    def.fkeys[i] = fk;
                    def.n_fkeys += 1;
                }
                Some("excl") => {
                    let Some((_, _, def, _, _)) = pending_def.as_mut() else {
                        return Err(CheckpointSetupError::Corrupt("excl outside table"));
                    };
                    if def.n_exclusions >= crate::storage::MAX_EXCLUSIONS {
                        return Err(CheckpointSetupError::Corrupt("too many excl lines"));
                    }
                    let timing: u8 = parse_field(words.next(), "excl timing")?;
                    let n_cols: usize = parse_field(words.next(), "excl ncols")?;
                    if n_cols == 0 || n_cols > crate::storage::MAX_INDEX_COLS {
                        return Err(CheckpointSetupError::Corrupt("bad excl ncols"));
                    }
                    let mut exclusion = crate::storage::ExclusionConstraint::EMPTY;
                    exclusion.timing = crate::storage::ConstraintTiming::from_code(timing)
                        .ok_or(CheckpointSetupError::Corrupt("bad excl timing"))?;
                    exclusion.n_cols = n_cols;
                    for position in 0..n_cols {
                        exclusion.columns[position] = parse_field(words.next(), "excl col")?;
                        let operator: u8 = parse_field(words.next(), "excl operator")?;
                        exclusion.operators[position] =
                            crate::storage::ExclusionOperator::from_code(operator)
                                .ok_or(CheckpointSetupError::Corrupt("bad excl operator"))?;
                    }
                    exclusion.name = sql_name(&decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("excl name missing"))?,
                    )?)?;
                    let predicate = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("excl predicate missing"))?;
                    if predicate != "-" {
                        let source = decode_hex_name(predicate)?;
                        let stored = crate::util::StackStr::from_str(&source);
                        if stored.is_truncated() {
                            return Err(CheckpointSetupError::Corrupt("excl predicate too long"));
                        }
                        exclusion.predicate = Some(stored);
                    }
                    let index = def.n_exclusions;
                    def.exclusions[index] = exclusion;
                    def.n_exclusions += 1;
                }
                Some("implicit_index") => {
                    let Some((_, _, definition, _, _)) = pending_def.as_mut() else {
                        return Err(CheckpointSetupError::Corrupt(
                            "implicit index outside table",
                        ));
                    };
                    let position: usize = parse_field(words.next(), "implicit index position")?;
                    let created_at: u64 = parse_field(words.next(), "implicit index creation")?;
                    let identity = crate::storage::RelationIdentity::parse(created_at)
                        .filter(|_| created_at <= metadata.catalog_sequence)
                        .ok_or(CheckpointSetupError::Corrupt(
                            "invalid implicit index creation identity",
                        ))?;
                    if !definition.attributes.validate(definition.n_columns) {
                        return Err(CheckpointSetupError::Corrupt(
                            "implicit index requires valid column attributes",
                        ));
                    }
                    let target = definition
                        .implicit_index_identities_mut()
                        .nth(position)
                        .ok_or(CheckpointSetupError::Corrupt(
                            "invalid implicit index position",
                        ))?;
                    if target.is_some() || words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt(
                            "duplicate or malformed implicit index identity",
                        ));
                    }
                    *target = Some(identity);
                }
                Some("constraint_identity") => {
                    let Some((_, _, definition, _, _)) = pending_def.as_mut() else {
                        return Err(CheckpointSetupError::Corrupt(
                            "constraint identity outside table",
                        ));
                    };
                    if storage.is_large_object_page_relation(
                        definition.schema.as_str(),
                        definition.name.as_str(),
                    ) {
                        return Err(CheckpointSetupError::Corrupt(
                            "internal storage table has no SQL constraint identities",
                        ));
                    }
                    let position: usize = parse_field(words.next(), "constraint position")?;
                    let created_at: u64 = parse_field(words.next(), "constraint creation")?;
                    let identity = crate::storage::ConstraintIdentity::parse(created_at)
                        .filter(|_| created_at <= metadata.catalog_sequence)
                        .ok_or(CheckpointSetupError::Corrupt(
                            "invalid constraint creation identity",
                        ))?;
                    if !definition.attributes.validate(definition.n_columns) {
                        return Err(CheckpointSetupError::Corrupt(
                            "constraint requires valid column attributes",
                        ));
                    }
                    let target = definition.constraint_identities_mut().nth(position).ok_or(
                        CheckpointSetupError::Corrupt("invalid constraint identity position"),
                    )?;
                    if target.is_some() || words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt(
                            "duplicate or malformed constraint identity",
                        ));
                    }
                    *target = Some(identity);
                }
                Some("dsst") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let mindex: usize = parse_field(words.next(), "dsst table")?;
                    let idx: usize = parse_field(words.next(), "dsst list index")?;
                    let count: u64 = parse_field(words.next(), "dsst count")?;
                    let crc: u32 = parse_field(words.next(), "dsst crc")?;
                    let index = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("dsst index"))?;
                    let filter = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("dsst filter"))?;
                    let roster = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("dsst roster"))?;
                    let handle = parse_dsst_handle(index, filter, roster, &mut words)?;
                    bssts.push((mindex, idx, count, crc, handle));
                }
                Some("vix") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let mindex: usize = parse_field(words.next(), "vix table")?;
                    let n_columns: usize = parse_field(words.next(), "vix columns")?;
                    if n_columns == 0 || n_columns > crate::storage::MAX_INDEX_COLS {
                        return Err(CheckpointSetupError::Corrupt("bad vix column count"));
                    }
                    let mut columns = [0u16; crate::storage::MAX_INDEX_COLS];
                    for column in columns.iter_mut().take(n_columns) {
                        *column = parse_field(words.next(), "vix column")?;
                    }
                    let include_mask = parse_field(words.next(), "vix include mask")?;
                    let index_created_at = match words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("vix index identity"))?
                    {
                        "-" => None,
                        value => Some(parse_field(Some(value), "vix index identity")?),
                    };
                    let roster = parse_block_id(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("vix roster"))?,
                    )?;
                    let entries = parse_field(words.next(), "vix entries")?;
                    let published_lsn = parse_field(words.next(), "vix lsn")?;
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt("trailing vix fields"));
                    }
                    value_indexes.push((
                        mindex,
                        columns,
                        n_columns,
                        index_created_at,
                        include_mask,
                        ValueIndexHandle {
                            roster,
                            entries,
                            published_lsn,
                        },
                    ));
                }
                Some("fdw") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let slot = parse_field(words.next(), "foreign wrapper slot")?;
                    let created_at: u64 = parse_field(words.next(), "foreign wrapper sequence")?;
                    let owner = decode_hex_name(words.next().ok_or(
                        CheckpointSetupError::Corrupt("foreign wrapper owner missing"),
                    )?)?;
                    let handler = match parse_field::<u8>(words.next(), "foreign wrapper handler")?
                    {
                        0 => crate::storage::foreign::ForeignDataHandler::None,
                        1 => crate::storage::foreign::ForeignDataHandler::Postgres,
                        _ => {
                            return Err(CheckpointSetupError::Corrupt(
                                "invalid foreign wrapper handler",
                            ));
                        }
                    };
                    let validator =
                        match parse_field::<u8>(words.next(), "foreign wrapper validator")? {
                            0 => crate::storage::foreign::ForeignDataValidator::None,
                            1 => crate::storage::foreign::ForeignDataValidator::Postgres,
                            _ => {
                                return Err(CheckpointSetupError::Corrupt(
                                    "invalid foreign wrapper validator",
                                ));
                            }
                        };
                    let name = sql_name(&decode_hex_name(words.next().ok_or(
                        CheckpointSetupError::Corrupt("foreign wrapper name missing"),
                    )?)?)?;
                    let option_count = parse_field(words.next(), "foreign wrapper options")?;
                    let options = parse_foreign_options(&mut words, option_count)?;
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt(
                            "trailing foreign wrapper fields",
                        ));
                    }
                    let owner = storage
                        .find_role(&owner)
                        .ok_or(CheckpointSetupError::Corrupt(
                            "foreign wrapper owner does not exist",
                        ))?;
                    storage
                        .restore_foreign_wrapper(
                            slot,
                            created_at,
                            crate::storage::foreign::ForeignDataWrapperDefinition {
                                name,
                                handler,
                                validator,
                                options,
                            },
                            owner as u16,
                        )
                        .map_err(|_| CheckpointSetupError::Corrupt("invalid foreign wrapper"))?;
                }
                Some("fsrv") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let slot = parse_field(words.next(), "foreign server slot")?;
                    let created_at: u64 = parse_field(words.next(), "foreign server sequence")?;
                    let owner = decode_hex_name(words.next().ok_or(
                        CheckpointSetupError::Corrupt("foreign server owner missing"),
                    )?)?;
                    let wrapper_name = decode_hex_name(words.next().ok_or(
                        CheckpointSetupError::Corrupt("foreign server wrapper missing"),
                    )?)?;
                    let server_type =
                        decode_optional_hex_name(words.next().ok_or(
                            CheckpointSetupError::Corrupt("foreign server type missing"),
                        )?)?;
                    let version = decode_optional_hex_name(words.next().ok_or(
                        CheckpointSetupError::Corrupt("foreign server version missing"),
                    )?)?;
                    let name =
                        sql_name(&decode_hex_name(words.next().ok_or(
                            CheckpointSetupError::Corrupt("foreign server name missing"),
                        )?)?)?;
                    let option_count = parse_field(words.next(), "foreign server options")?;
                    let options = parse_foreign_options(&mut words, option_count)?;
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt(
                            "trailing foreign server fields",
                        ));
                    }
                    let owner = storage
                        .find_role(&owner)
                        .ok_or(CheckpointSetupError::Corrupt(
                            "foreign server owner does not exist",
                        ))?;
                    let wrapper = storage
                        .foreign_wrapper(&wrapper_name, 0)
                        .ok_or(CheckpointSetupError::Corrupt(
                            "foreign server wrapper does not exist",
                        ))?
                        .0;
                    storage
                        .restore_foreign_server(
                            slot,
                            created_at,
                            crate::storage::foreign::ForeignServerDefinition {
                                name,
                                wrapper: wrapper as u16,
                                server_type: server_type.map(|value| StackStr::from_str(&value)),
                                version: version.map(|value| StackStr::from_str(&value)),
                                options,
                            },
                            owner as u16,
                        )
                        .map_err(|_| CheckpointSetupError::Corrupt("invalid foreign server"))?;
                }
                Some("fum") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let slot = parse_field(words.next(), "user mapping slot")?;
                    let created_at: u64 = parse_field(words.next(), "user mapping sequence")?;
                    let server_name =
                        decode_hex_name(words.next().ok_or(CheckpointSetupError::Corrupt(
                            "user mapping server missing",
                        ))?)?;
                    let user_kind: u8 = parse_field(words.next(), "user mapping kind")?;
                    let user_name = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("user mapping role missing"))?,
                    )?;
                    let option_count = parse_field(words.next(), "user mapping options")?;
                    let options = parse_foreign_options(&mut words, option_count)?;
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt(
                            "trailing user mapping fields",
                        ));
                    }
                    let server = storage
                        .foreign_server(&server_name, 0)
                        .ok_or(CheckpointSetupError::Corrupt(
                            "user mapping server does not exist",
                        ))?
                        .0;
                    let user = match user_kind {
                        0 if user_name.is_empty() => {
                            crate::storage::foreign::ForeignMappingUser::Public
                        }
                        1 => crate::storage::foreign::ForeignMappingUser::Role(
                            storage
                                .find_role(&user_name)
                                .ok_or(CheckpointSetupError::Corrupt(
                                    "user mapping role does not exist",
                                ))? as u16,
                        ),
                        _ => {
                            return Err(CheckpointSetupError::Corrupt("invalid user mapping kind"));
                        }
                    };
                    storage
                        .restore_foreign_user_mapping(
                            slot,
                            created_at,
                            crate::storage::foreign::UserMappingDefinition {
                                server: server as u16,
                                user,
                                options,
                            },
                        )
                        .map_err(|_| CheckpointSetupError::Corrupt("invalid user mapping"))?;
                }
                Some("ftab") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let slot = parse_field(words.next(), "foreign table slot")?;
                    let created_at: u64 = parse_field(words.next(), "foreign table sequence")?;
                    let schema = decode_hex_name(words.next().ok_or(
                        CheckpointSetupError::Corrupt("foreign table schema missing"),
                    )?)?;
                    let table_name = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("foreign table name missing"))?,
                    )?;
                    let server_name = decode_hex_name(words.next().ok_or(
                        CheckpointSetupError::Corrupt("foreign table server missing"),
                    )?)?;
                    let option_count = parse_field(words.next(), "foreign table options")?;
                    let options = parse_foreign_options(&mut words, option_count)?;
                    let column_option_count: usize =
                        parse_field(words.next(), "foreign column options")?;
                    if column_option_count > crate::storage::foreign::MAX_FOREIGN_COLUMN_OPTIONS {
                        return Err(CheckpointSetupError::Corrupt(
                            "too many foreign column options",
                        ));
                    }
                    let mut column_options = crate::storage::foreign::ForeignColumnOptions::EMPTY;
                    for _ in 0..column_option_count {
                        let column = parse_field(words.next(), "foreign option column")?;
                        let name = decode_hex_name(words.next().ok_or(
                            CheckpointSetupError::Corrupt("foreign column option name missing"),
                        )?)?;
                        let value = decode_hex_name(words.next().ok_or(
                            CheckpointSetupError::Corrupt("foreign column option value missing"),
                        )?)?;
                        let mut one = crate::storage::foreign::ForeignOptions::EMPTY;
                        one.restore_option(&name, &value).map_err(|_| {
                            CheckpointSetupError::Corrupt("invalid foreign column option")
                        })?;
                        column_options.append(column, one).map_err(|_| {
                            CheckpointSetupError::Corrupt("invalid foreign column options")
                        })?;
                    }
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt(
                            "trailing foreign table fields",
                        ));
                    }
                    let table = storage.find_visible(&schema, &table_name, 0).ok_or(
                        CheckpointSetupError::Corrupt("foreign table relation does not exist"),
                    )?;
                    if storage.table_def(table, 0).kind != crate::storage::TableKind::Foreign {
                        return Err(CheckpointSetupError::Corrupt(
                            "foreign binding targets local table",
                        ));
                    }
                    let server = storage
                        .foreign_server(&server_name, 0)
                        .ok_or(CheckpointSetupError::Corrupt(
                            "foreign table server does not exist",
                        ))?
                        .0;
                    storage
                        .restore_foreign_table_binding(
                            slot,
                            created_at,
                            crate::storage::foreign::ForeignTableDefinition {
                                table: table as u16,
                                server: server as u16,
                                options,
                                column_options,
                            },
                        )
                        .map_err(|_| CheckpointSetupError::Corrupt("invalid foreign table"))?;
                }
                Some("vw13") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    load_view(storage, line, metadata.catalog_sequence)?;
                }
                Some("mv6") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    load_matview(storage, line, metadata.catalog_sequence)?;
                }
                Some("pub") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    load_publication(storage, line)?;
                }
                Some("rslot") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    load_replication_slot(storage, line)?;
                }
                Some("sub") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    load_subscription(storage, line)?;
                }
                Some("subrel") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    load_subscription_relation(storage, line)?;
                }
                Some("rtn2") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let position = parse_catalog_position(&mut words, metadata.catalog_sequence)?;
                    let owner = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("routine owner missing"))?,
                    )?;
                    let result_code: u8 = parse_field(words.next(), "routine result type")?;
                    let argument_count: usize =
                        parse_field(words.next(), "routine argument count")?;
                    if argument_count > crate::storage::MAX_ROUTINE_ARGUMENTS {
                        return Err(CheckpointSetupError::Corrupt("too many routine arguments"));
                    }
                    let schema =
                        sql_name(&decode_hex_name(words.next().ok_or(
                            CheckpointSetupError::Corrupt("routine schema missing"),
                        )?)?)?;
                    let name = sql_name(&decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("routine name missing"))?,
                    )?)?;
                    let body = StackStr::<{ crate::storage::ROUTINE_SQL_MAX }>::from_str(
                        &decode_hex_name(
                            words
                                .next()
                                .ok_or(CheckpointSetupError::Corrupt("routine body missing"))?,
                        )?,
                    );
                    if body.is_truncated() {
                        return Err(CheckpointSetupError::Corrupt("routine body too long"));
                    }
                    let mut arguments = [crate::storage::RoutineArgumentDef::EMPTY;
                        crate::storage::MAX_ROUTINE_ARGUMENTS];
                    for argument in &mut arguments[..argument_count] {
                        let argument_name = words.next().ok_or(CheckpointSetupError::Corrupt(
                            "routine argument name missing",
                        ))?;
                        let type_word = words.next();
                        argument.name = sql_name(&decode_hex_name(argument_name)?)?;
                        let type_code: u8 = parse_field(type_word, "routine argument type")?;
                        argument.ctype = ColType::from_code(type_code).ok_or(
                            CheckpointSetupError::Corrupt("invalid routine argument type"),
                        )?;
                        let schema = words.next().ok_or(CheckpointSetupError::Corrupt(
                            "routine argument type schema missing",
                        ))?;
                        let name = words.next().ok_or(CheckpointSetupError::Corrupt(
                            "routine argument type name missing",
                        ))?;
                        argument.user_type = if schema == "-" && name == "-" {
                            None
                        } else {
                            Some(
                                crate::storage::UserTypeName::from_storage(
                                    sql_name(&decode_hex_name(schema)?)?,
                                    &decode_hex_name(name)?,
                                )
                                .map_err(|_| {
                                    CheckpointSetupError::Corrupt("invalid stored type identity")
                                })?,
                            )
                        };
                    }
                    let parameter_count: usize =
                        parse_field(words.next(), "routine parameter count")?;
                    if parameter_count > crate::storage::MAX_ROUTINE_ARGUMENTS {
                        return Err(CheckpointSetupError::Corrupt("too many routine parameters"));
                    }
                    let mut parameters = [crate::storage::RoutineParameterDef::EMPTY;
                        crate::storage::MAX_ROUTINE_ARGUMENTS];
                    for parameter in &mut parameters[..parameter_count] {
                        parameter.name = sql_name(&decode_hex_name(words.next().ok_or(
                            CheckpointSetupError::Corrupt("routine parameter name missing"),
                        )?)?)?;
                        let type_code: u8 = parse_field(words.next(), "routine parameter type")?;
                        parameter.ctype = ColType::from_code(type_code).ok_or(
                            CheckpointSetupError::Corrupt("invalid routine parameter type"),
                        )?;
                        let schema = words.next().ok_or(CheckpointSetupError::Corrupt(
                            "routine parameter type schema missing",
                        ))?;
                        let name = words.next().ok_or(CheckpointSetupError::Corrupt(
                            "routine parameter type name missing",
                        ))?;
                        parameter.user_type = if schema == "-" && name == "-" {
                            None
                        } else {
                            Some(
                                crate::storage::UserTypeName::from_storage(
                                    sql_name(&decode_hex_name(schema)?)?,
                                    &decode_hex_name(name)?,
                                )
                                .map_err(|_| {
                                    CheckpointSetupError::Corrupt("invalid stored type identity")
                                })?,
                            )
                        };
                        let mode: u8 = parse_field(words.next(), "routine parameter mode")?;
                        let default_word = words.next().ok_or(CheckpointSetupError::Corrupt(
                            "routine parameter default missing",
                        ))?;
                        let default = if default_word == "-" {
                            None
                        } else {
                            let decoded = decode_hex_name(default_word)?;
                            let stored = StackStr::from_str(&decoded);
                            if stored.is_truncated() {
                                return Err(CheckpointSetupError::Corrupt(
                                    "routine parameter default too long",
                                ));
                            }
                            Some(stored)
                        };
                        parameter.mode =
                            crate::storage::RoutineParameterMode::from_code(mode, default).ok_or(
                                CheckpointSetupError::Corrupt("invalid routine parameter mode"),
                            )?;
                    }
                    let config_count: usize =
                        parse_field(words.next(), "routine configuration count")?;
                    if config_count > crate::storage::MAX_ROUTINE_CONFIGS {
                        return Err(CheckpointSetupError::Corrupt(
                            "too many routine configurations",
                        ));
                    }
                    let mut configs =
                        [crate::storage::RoutineConfig::EMPTY; crate::storage::MAX_ROUTINE_CONFIGS];
                    for config in &mut configs[..config_count] {
                        config.name = sql_name(&decode_hex_name(words.next().ok_or(
                            CheckpointSetupError::Corrupt("routine configuration name missing"),
                        )?)?)?;
                        let decoded = decode_hex_name(words.next().ok_or(
                            CheckpointSetupError::Corrupt("routine configuration value missing"),
                        )?)?;
                        config.value = StackStr::from_str(&decoded);
                        if config.value.is_truncated() {
                            return Err(CheckpointSetupError::Corrupt(
                                "routine configuration value too long",
                            ));
                        }
                    }
                    let strict = match parse_field::<u8>(words.next(), "routine strictness")? {
                        0 => false,
                        1 => true,
                        _ => {
                            return Err(CheckpointSetupError::Corrupt(
                                "invalid routine strictness",
                            ));
                        }
                    };
                    let volatility = crate::storage::RoutineVolatility::from_code(parse_field(
                        words.next(),
                        "routine volatility",
                    )?)
                    .ok_or(CheckpointSetupError::Corrupt("invalid routine volatility"))?;
                    let parallel = crate::storage::RoutineParallel::from_code(parse_field(
                        words.next(),
                        "routine parallel safety",
                    )?)
                    .ok_or(CheckpointSetupError::Corrupt(
                        "invalid routine parallel safety",
                    ))?;
                    let body_kind = crate::storage::RoutineBodyKind::from_code(parse_field(
                        words.next(),
                        "routine body kind",
                    )?)
                    .ok_or(CheckpointSetupError::Corrupt("invalid routine body kind"))?;
                    let language = crate::storage::RoutineLanguage::from_code(parse_field(
                        words.next(),
                        "routine language",
                    )?)
                    .ok_or(CheckpointSetupError::Corrupt("invalid routine language"))?;
                    let security_definer =
                        match parse_field::<u8>(words.next(), "routine security")? {
                            0 => false,
                            1 => true,
                            _ => {
                                return Err(CheckpointSetupError::Corrupt(
                                    "invalid routine security",
                                ));
                            }
                        };
                    let leakproof = match parse_field::<u8>(words.next(), "routine leakproof")? {
                        0 => false,
                        1 => true,
                        _ => {
                            return Err(CheckpointSetupError::Corrupt("invalid routine leakproof"));
                        }
                    };
                    let parse_estimate = |word: Option<&str>, missing| {
                        let bits: u64 = parse_field(word, missing)?;
                        if bits == 0 {
                            return Ok(None);
                        }
                        let value = f64::from_bits(bits);
                        if !value.is_finite() || value <= 0.0 {
                            return Err(CheckpointSetupError::Corrupt("invalid routine estimate"));
                        }
                        Ok(Some(bits))
                    };
                    let attributes = crate::storage::RoutineAttributes {
                        strict,
                        volatility,
                        parallel,
                        security_definer,
                        leakproof,
                        cost_bits: parse_estimate(words.next(), "routine cost")?,
                        rows_bits: parse_estimate(words.next(), "routine rows")?,
                    };
                    let mut result_columns = [crate::storage::RoutineArgumentDef::EMPTY;
                        crate::storage::MAX_ROUTINE_OUTPUT_COLUMNS];
                    let mut result_column_count = 0;
                    let kind_code = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("routine kind missing"))?;
                    let result_schema = words.next().ok_or(CheckpointSetupError::Corrupt(
                        "routine result type schema missing",
                    ))?;
                    let result_name = words.next().ok_or(CheckpointSetupError::Corrupt(
                        "routine result type name missing",
                    ))?;
                    let result_user_type = if result_schema == "-" && result_name == "-" {
                        None
                    } else {
                        Some(
                            crate::storage::UserTypeName::from_storage(
                                sql_name(&decode_hex_name(result_schema)?)?,
                                &decode_hex_name(result_name)?,
                            )
                            .map_err(|_| {
                                CheckpointSetupError::Corrupt("invalid stored type identity")
                            })?,
                        )
                    };
                    let result = crate::storage::RoutineResult {
                        ctype: ColType::from_code(result_code)
                            .ok_or(CheckpointSetupError::Corrupt("invalid routine result type"))?,
                        user_type: result_user_type,
                    };
                    let code: u8 = parse_field(Some(kind_code), "routine kind")?;
                    let kind = {
                        if matches!(code, 3 | 6 | 7) {
                            result_column_count =
                                parse_field(words.next(), "routine result column count")?;
                            if result_column_count > crate::storage::MAX_ROUTINE_OUTPUT_COLUMNS {
                                return Err(CheckpointSetupError::Corrupt(
                                    "too many routine result columns",
                                ));
                            }
                            for column in &mut result_columns[..result_column_count] {
                                column.name = sql_name(&decode_hex_name(words.next().ok_or(
                                    CheckpointSetupError::Corrupt(
                                        "routine result column name missing",
                                    ),
                                )?)?)?;
                                let type_code: u8 =
                                    parse_field(words.next(), "routine result column type")?;
                                column.ctype = ColType::from_code(type_code).ok_or(
                                    CheckpointSetupError::Corrupt(
                                        "invalid routine result column type",
                                    ),
                                )?;
                                let schema = words.next().ok_or(CheckpointSetupError::Corrupt(
                                    "routine result column type schema missing",
                                ))?;
                                let name = words.next().ok_or(CheckpointSetupError::Corrupt(
                                    "routine result column type name missing",
                                ))?;
                                column.user_type = if schema == "-" && name == "-" {
                                    None
                                } else {
                                    Some(
                                        crate::storage::UserTypeName::from_storage(
                                            sql_name(&decode_hex_name(schema)?)?,
                                            &decode_hex_name(name)?,
                                        )
                                        .map_err(|_| {
                                            CheckpointSetupError::Corrupt(
                                                "invalid stored type identity",
                                            )
                                        })?,
                                    )
                                };
                            }
                            crate::storage::RoutineKind::from_wire_code(code, result)
                                .ok_or(CheckpointSetupError::Corrupt("invalid routine kind"))?
                        } else if code == 5 {
                            let aggregate =
                                crate::storage::AggregateRoutine::decode_wire(body.as_str())
                                    .ok_or(CheckpointSetupError::Corrupt(
                                        "invalid aggregate definition",
                                    ))?;
                            crate::storage::RoutineKind::Aggregate(aggregate)
                        } else {
                            crate::storage::RoutineKind::from_wire_code(code, result)
                                .ok_or(CheckpointSetupError::Corrupt("invalid routine kind"))?
                        }
                    };
                    let creation_path =
                        StackStr::<128>::from_str(&decode_hex_name(words.next().ok_or(
                            CheckpointSetupError::Corrupt("routine creation path missing"),
                        )?)?);
                    if creation_path.is_truncated() {
                        return Err(CheckpointSetupError::Corrupt(
                            "routine creation path too long",
                        ));
                    }
                    let dependencies = parse_stored_query_dependencies(&mut words)?;
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt("malformed routine record"));
                    }
                    let owner = storage
                        .find_role(&owner)
                        .ok_or(CheckpointSetupError::Corrupt(
                            "routine owner does not exist",
                        ))?;
                    let slot = storage
                        .create_routine(
                            crate::storage::RoutineSpec {
                                identity: crate::storage::RoutineIdentity::Preserve {
                                    position,
                                    ownership: crate::storage::Ownership {
                                        owner: owner as u16,
                                        pending: None,
                                    },
                                },
                                schema,
                                name,
                                arguments,
                                argument_count,
                                parameters,
                                parameter_count,
                                kind,
                                result_columns,
                                result_column_count,
                                language,
                                attributes,
                                configs,
                                config_count,
                                body_kind,
                                body: if code == 5 { StackStr::new() } else { body },
                                creation_path,
                                dependencies,
                            },
                            0,
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest routine rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                    storage.finish_routine_change(slot, crate::storage::CatalogDdlDecision::Commit);
                }
                Some("evt") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let slot: usize = parse_field(words.next(), "event trigger slot")?;
                    let created_at = parse_field(words.next(), "event trigger created_at")?;
                    let name =
                        sql_name(&decode_hex_name(words.next().ok_or(
                            CheckpointSetupError::Corrupt("event trigger name missing"),
                        )?)?)?;
                    let event = crate::sql::ast::EventTriggerEvent::from_code(parse_field(
                        words.next(),
                        "event trigger event",
                    )?)
                    .ok_or(CheckpointSetupError::Corrupt("invalid event trigger event"))?;
                    let function_schema = decode_hex_name(words.next().ok_or(
                        CheckpointSetupError::Corrupt("event trigger function schema missing"),
                    )?)?;
                    let function_name = decode_hex_name(words.next().ok_or(
                        CheckpointSetupError::Corrupt("event trigger function name missing"),
                    )?)?;
                    let function = storage
                        .routine_slot_by_signature(&function_schema, &function_name, &[], 0)
                        .ok_or(CheckpointSetupError::Corrupt(
                            "event trigger function missing",
                        ))? as u16;
                    let enabled = crate::storage::TriggerEnabled::from_code(parse_field(
                        words.next(),
                        "event trigger enabled mode",
                    )?)
                    .ok_or(CheckpointSetupError::Corrupt(
                        "invalid event trigger enabled mode",
                    ))?;
                    let owner_name =
                        decode_hex_name(words.next().ok_or(CheckpointSetupError::Corrupt(
                            "event trigger owner missing",
                        ))?)?;
                    let owner =
                        storage
                            .find_role(&owner_name)
                            .ok_or(CheckpointSetupError::Corrupt(
                                "event trigger owner does not exist",
                            ))? as u16;
                    let tag_count: usize = parse_field(words.next(), "event trigger tag count")?;
                    if tag_count > crate::storage::MAX_EVENT_TRIGGER_TAGS {
                        return Err(CheckpointSetupError::Corrupt("too many event trigger tags"));
                    }
                    let mut decoded_tags =
                        [StackStr::<{ crate::storage::EVENT_TRIGGER_TAG_MAX }>::new();
                            crate::storage::MAX_EVENT_TRIGGER_TAGS];
                    for decoded in decoded_tags.iter_mut().take(tag_count) {
                        *decoded =
                            StackStr::from_str(&decode_hex_name(words.next().ok_or(
                                CheckpointSetupError::Corrupt("event trigger tag missing"),
                            )?)?);
                        if decoded.is_truncated() {
                            return Err(CheckpointSetupError::Corrupt(
                                "event trigger tag too long",
                            ));
                        }
                    }
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt(
                            "trailing event trigger fields",
                        ));
                    }
                    let mut tag_values = [""; crate::storage::MAX_EVENT_TRIGGER_TAGS];
                    for (index, decoded) in decoded_tags.iter().take(tag_count).enumerate() {
                        tag_values[index] = decoded.as_str();
                    }
                    let tags = crate::storage::EventTriggerTags::parse(&tag_values[..tag_count])
                        .map_err(|_| CheckpointSetupError::Corrupt("invalid event trigger tags"))?;
                    storage
                        .replay_event_trigger(
                            slot,
                            created_at,
                            crate::storage::EventTriggerDefinition {
                                name,
                                event,
                                function,
                                tags,
                                enabled,
                                ownership: crate::storage::Ownership {
                                    owner,
                                    pending: None,
                                },
                            },
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest event trigger rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("rul") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let slot: usize = parse_field(words.next(), "rule slot")?;
                    let created_at = parse_field(words.next(), "rule created_at")?;
                    let target_kind: u8 = parse_field(words.next(), "rule target kind")?;
                    let schema = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("rule schema missing"))?,
                    )?;
                    let relation = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("rule relation missing"))?,
                    )?;
                    let name = sql_name(&decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("rule name missing"))?,
                    )?)?;
                    let event = crate::storage::RewriteEvent::from_code(parse_field(
                        words.next(),
                        "rule event",
                    )?)
                    .ok_or(CheckpointSetupError::Corrupt("invalid rule event"))?;
                    let mode = crate::storage::RewriteMode::from_code(parse_field(
                        words.next(),
                        "rule mode",
                    )?)
                    .ok_or(CheckpointSetupError::Corrupt("invalid rule mode"))?;
                    let enabled = crate::storage::RuleEnabled::from_code(parse_field(
                        words.next(),
                        "rule enabled mode",
                    )?)
                    .ok_or(CheckpointSetupError::Corrupt("invalid rule enabled mode"))?;
                    let source =
                        StackStr::<{ crate::storage::RULE_SQL_MAX }>::from_str(&decode_hex_name(
                            words
                                .next()
                                .ok_or(CheckpointSetupError::Corrupt("rule source missing"))?,
                        )?);
                    if source.is_truncated() {
                        return Err(CheckpointSetupError::Corrupt("rule source too long"));
                    }
                    let condition_start: u16 = parse_field(words.next(), "rule condition start")?;
                    let condition_len: u16 = parse_field(words.next(), "rule condition length")?;
                    let condition =
                        (condition_start != u16::MAX).then_some(crate::storage::RuleTextSpan {
                            start: condition_start,
                            len: condition_len,
                        });
                    if condition_start == u16::MAX && condition_len != 0 {
                        return Err(CheckpointSetupError::Corrupt("invalid rule condition span"));
                    }
                    let action_count: usize = parse_field(words.next(), "rule action count")?;
                    if action_count > crate::storage::MAX_RULE_ACTIONS {
                        return Err(CheckpointSetupError::Corrupt("too many rule actions"));
                    }
                    let mut actions = [crate::storage::RuleTextSpan { start: 0, len: 0 };
                        crate::storage::MAX_RULE_ACTIONS];
                    for action in &mut actions[..action_count] {
                        action.start = parse_field(words.next(), "rule action start")?;
                        action.len = parse_field(words.next(), "rule action length")?;
                    }
                    let returning_action: u16 = parse_field(words.next(), "rule returning action")?;
                    let returning_action = match returning_action {
                        u16::MAX => None,
                        index if usize::from(index) < action_count => Some(index as u8),
                        _ => {
                            return Err(CheckpointSetupError::Corrupt(
                                "invalid rule returning action",
                            ));
                        }
                    };
                    let creation_path =
                        StackStr::<128>::from_str(&decode_hex_name(words.next().ok_or(
                            CheckpointSetupError::Corrupt("rule creation path missing"),
                        )?)?);
                    if creation_path.is_truncated() {
                        return Err(CheckpointSetupError::Corrupt("rule creation path too long"));
                    }
                    let dependencies = parse_stored_query_dependencies(&mut words)?;
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt("trailing rule fields"));
                    }
                    let valid_span = |span: crate::storage::RuleTextSpan| {
                        let start = usize::from(span.start);
                        start.checked_add(usize::from(span.len)).is_some_and(|end| {
                            end <= source.as_str().len()
                                && source.as_str().is_char_boundary(start)
                                && source.as_str().is_char_boundary(end)
                        })
                    };
                    if condition.is_some_and(|span| !valid_span(span))
                        || actions[..action_count]
                            .iter()
                            .any(|span| !valid_span(*span))
                    {
                        return Err(CheckpointSetupError::Corrupt("invalid rule text span"));
                    }
                    let target = match target_kind {
                        0 => storage
                            .find_table(&schema, &relation)
                            .and_then(|slot| u16::try_from(slot).ok())
                            .map(crate::storage::RuleTarget::Table),
                        1 => storage
                            .views_visible_to(0)
                            .find(|(_, view)| {
                                view.schema.as_str() == schema.as_str()
                                    && view.name.as_str() == relation.as_str()
                            })
                            .and_then(|(slot, _)| u16::try_from(slot).ok())
                            .map(crate::storage::RuleTarget::View),
                        _ => None,
                    }
                    .ok_or(CheckpointSetupError::Corrupt("rule target missing"))?;
                    storage
                        .replay_rule(
                            slot,
                            created_at,
                            crate::storage::RuleDefinition {
                                name,
                                target,
                                event,
                                mode,
                                enabled,
                                source,
                                condition,
                                actions,
                                action_count: action_count as u8,
                                returning_action,
                                creation_path,
                            },
                            dependencies,
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest rewrite rule rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("cst") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let identity = parse_catalog_position(&mut words, metadata.catalog_sequence)?;
                    let source = manifest_routine_result(&mut words)?;
                    let target = manifest_routine_result(&mut words)?;
                    let method_code: u8 = parse_field(words.next(), "cast method")?;
                    let function_oid: i32 = parse_field(words.next(), "cast function")?;
                    let method = match method_code {
                        b'f' if storage.routine_slot_by_oid(function_oid, 0).is_some() => {
                            crate::storage::CastMethod::Function(function_oid)
                        }
                        b'b' if function_oid == 0 => crate::storage::CastMethod::Binary,
                        b'i' if function_oid == 0 => crate::storage::CastMethod::InOut,
                        _ => return Err(CheckpointSetupError::Corrupt("invalid cast method")),
                    };
                    let context = crate::storage::CastContext::from_code(parse_field(
                        words.next(),
                        "cast context",
                    )?)
                    .ok_or(CheckpointSetupError::Corrupt("invalid cast context"))?;
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt("trailing cast fields"));
                    }
                    storage
                        .create_cast_from_image(
                            identity,
                            crate::storage::CastDef {
                                database: crate::storage::DatabaseOid::POSTGRES,
                                created_at: identity.created_at(),
                                source,
                                target,
                                method,
                                context,
                                ddl_state: crate::storage::CatalogDdlState::Absent,
                            },
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest cast rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("tsobj") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let slot: usize = parse_field(words.next(), "text-search slot")?;
                    let created_at = parse_field(words.next(), "text-search created_at")?;
                    let kind = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("text-search kind missing"))?;
                    let schema =
                        sql_name(&decode_hex_name(words.next().ok_or(
                            CheckpointSetupError::Corrupt("text-search schema missing"),
                        )?)?)?;
                    let name =
                        sql_name(&decode_hex_name(words.next().ok_or(
                            CheckpointSetupError::Corrupt("text-search name missing"),
                        )?)?)?;
                    let oid = parse_field(words.next(), "text-search OID")?;
                    if oid <= 0 {
                        return Err(CheckpointSetupError::Corrupt("invalid text-search OID"));
                    }
                    let behavior = |word: Option<&str>| -> Result<
                        crate::storage::TextSearchDictionaryBehavior,
                        CheckpointSetupError,
                    > {
                        Ok(
                            match parse_field::<u8>(word, "text-search dictionary behavior")? {
                                0 => crate::storage::TextSearchDictionaryBehavior::Simple {
                                    accept: true,
                                },
                                1 => crate::storage::TextSearchDictionaryBehavior::Simple {
                                    accept: false,
                                },
                                2 => crate::storage::TextSearchDictionaryBehavior::EnglishStem,
                                _ => {
                                    return Err(CheckpointSetupError::Corrupt(
                                        "invalid text-search dictionary behavior",
                                    ));
                                }
                            },
                        )
                    };
                    let definition = match kind {
                        "p" => crate::storage::TextSearchDefinition::Parser {
                            schema,
                            name,
                            oid,
                            start: parse_field(words.next(), "text-search parser start")?,
                            gettoken: parse_field(words.next(), "text-search parser token")?,
                            end: parse_field(words.next(), "text-search parser end")?,
                            headline: parse_field(words.next(), "text-search parser headline")?,
                            lextypes: parse_field(words.next(), "text-search parser lextypes")?,
                        },
                        "t" => crate::storage::TextSearchDefinition::Template {
                            schema,
                            name,
                            oid,
                            init: parse_field(words.next(), "text-search template init")?,
                            lexize: parse_field(words.next(), "text-search template lexize")?,
                            behavior: behavior(words.next())?,
                        },
                        "d" => {
                            let owner = parse_field(words.next(), "text-search dictionary owner")?;
                            if storage.role_slot_by_oid(owner, 0).is_none() {
                                return Err(CheckpointSetupError::Corrupt(
                                    "text-search dictionary owner missing",
                                ));
                            }
                            let template =
                                parse_field(words.next(), "text-search dictionary template")?;
                            let decoded = decode_hex_name(words.next().ok_or(
                                CheckpointSetupError::Corrupt(
                                    "text-search dictionary options missing",
                                ),
                            )?)?;
                            let options = StackStr::<512>::from_str(&decoded);
                            if options.is_truncated() {
                                return Err(CheckpointSetupError::Corrupt(
                                    "text-search dictionary options too long",
                                ));
                            }
                            crate::storage::TextSearchDefinition::Dictionary {
                                schema,
                                name,
                                oid,
                                owner,
                                template,
                                options,
                                behavior: behavior(words.next())?,
                            }
                        }
                        "c" => {
                            let owner =
                                parse_field(words.next(), "text-search configuration owner")?;
                            if storage.role_slot_by_oid(owner, 0).is_none() {
                                return Err(CheckpointSetupError::Corrupt(
                                    "text-search configuration owner missing",
                                ));
                            }
                            let parser =
                                parse_field(words.next(), "text-search configuration parser")?;
                            let mut mappings = crate::storage::TextSearchMappings::EMPTY;
                            for token in 0..crate::storage::TEXT_SEARCH_TOKEN_TYPES {
                                let count: usize =
                                    parse_field(words.next(), "text-search mapping count")?;
                                if count > crate::storage::TEXT_SEARCH_DICTIONARIES_PER_TOKEN {
                                    return Err(CheckpointSetupError::Corrupt(
                                        "too many text-search mapping dictionaries",
                                    ));
                                }
                                mappings.counts[token] = count as u8;
                                for dictionary in &mut mappings.dictionaries[token][..count] {
                                    *dictionary = parse_field(
                                        words.next(),
                                        "text-search mapping dictionary",
                                    )?;
                                    if *dictionary <= 0 {
                                        return Err(CheckpointSetupError::Corrupt(
                                            "invalid text-search mapping dictionary",
                                        ));
                                    }
                                }
                            }
                            crate::storage::TextSearchDefinition::Configuration {
                                schema,
                                name,
                                oid,
                                owner,
                                parser,
                                mappings,
                            }
                        }
                        _ => return Err(CheckpointSetupError::Corrupt("invalid text-search kind")),
                    };
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt("trailing text-search fields"));
                    }
                    match definition {
                        crate::storage::TextSearchDefinition::Dictionary { template, .. } => {
                            if storage
                                .text_search_slot_by_oid(
                                    crate::sql::ast::TextSearchObjectKind::Template,
                                    template,
                                    0,
                                )
                                .is_none()
                            {
                                return Err(CheckpointSetupError::Corrupt(
                                    "text-search dictionary template missing",
                                ));
                            }
                        }
                        crate::storage::TextSearchDefinition::Configuration {
                            parser,
                            mappings,
                            ..
                        } => {
                            if storage
                                .text_search_slot_by_oid(
                                    crate::sql::ast::TextSearchObjectKind::Parser,
                                    parser,
                                    0,
                                )
                                .is_none()
                            {
                                return Err(CheckpointSetupError::Corrupt(
                                    "text-search configuration parser missing",
                                ));
                            }
                            for token in 0..crate::storage::TEXT_SEARCH_TOKEN_TYPES {
                                for dictionary in &mappings.dictionaries[token]
                                    [..usize::from(mappings.counts[token])]
                                {
                                    if storage
                                        .text_search_slot_by_oid(
                                            crate::sql::ast::TextSearchObjectKind::Dictionary,
                                            *dictionary,
                                            0,
                                        )
                                        .is_none()
                                    {
                                        return Err(CheckpointSetupError::Corrupt(
                                            "text-search mapping dictionary missing",
                                        ));
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    storage
                        .replay_text_search_object(slot, created_at, definition)
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest text-search object rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("coll") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let slot = parse_field(words.next(), "collation slot")?;
                    let created_at = parse_field(words.next(), "collation created_at")?;
                    let schema =
                        sql_name(&decode_hex_name(words.next().ok_or(
                            CheckpointSetupError::Corrupt("collation schema missing"),
                        )?)?)?;
                    let name =
                        sql_name(&decode_hex_name(words.next().ok_or(
                            CheckpointSetupError::Corrupt("collation name missing"),
                        )?)?)?;
                    let owner: i32 = parse_field(words.next(), "collation owner")?;
                    if storage.role_slot_by_oid(owner, 0).is_none() {
                        return Err(CheckpointSetupError::Corrupt("collation owner missing"));
                    }
                    let provider = match parse_field::<u8>(words.next(), "collation provider")? {
                        b'd' => crate::storage::CollationProvider::Default,
                        b'b' => crate::storage::CollationProvider::Builtin,
                        b'c' => crate::storage::CollationProvider::Libc,
                        b'i' => crate::storage::CollationProvider::Icu,
                        _ => {
                            return Err(CheckpointSetupError::Corrupt(
                                "invalid collation provider",
                            ));
                        }
                    };
                    let deterministic =
                        match parse_field::<u8>(words.next(), "collation deterministic")? {
                            0 => false,
                            1 => true,
                            _ => {
                                return Err(CheckpointSetupError::Corrupt(
                                    "invalid collation determinism",
                                ));
                            }
                        };
                    let encoding =
                        match parse_field::<u8>(words.next(), "collation encoding")? {
                            255 => None,
                            code => Some(crate::storage::PgEncoding::from_code(code).ok_or(
                                CheckpointSetupError::Corrupt("invalid collation encoding"),
                            )?),
                        };
                    let fixed = |value: Option<&str>, missing, too_long| {
                        let decoded =
                            decode_hex_name(value.ok_or(CheckpointSetupError::Corrupt(missing))?)?;
                        let fixed = StackStr::from_str(&decoded);
                        (!fixed.is_truncated())
                            .then_some(fixed)
                            .ok_or(CheckpointSetupError::Corrupt(too_long))
                    };
                    let collate = fixed(
                        words.next(),
                        "collation collate missing",
                        "collation collate too long",
                    )?;
                    let ctype = fixed(
                        words.next(),
                        "collation ctype missing",
                        "collation ctype too long",
                    )?;
                    let locale = fixed(
                        words.next(),
                        "collation locale missing",
                        "collation locale too long",
                    )?;
                    let rules = fixed(
                        words.next(),
                        "collation rules missing",
                        "collation rules too long",
                    )?;
                    let decoded_version = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("collation version missing"))?,
                    )?;
                    let version = StackStr::<64>::from_str(&decoded_version);
                    if version.is_truncated() {
                        return Err(CheckpointSetupError::Corrupt("collation version too long"));
                    }
                    let behavior = match parse_field::<u8>(words.next(), "collation behavior")? {
                        0 => crate::storage::CollationBehavior::Bytewise,
                        1 => crate::storage::CollationBehavior::Database,
                        _ => {
                            return Err(CheckpointSetupError::Corrupt(
                                "invalid collation behavior",
                            ));
                        }
                    };
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt("trailing collation fields"));
                    }
                    storage
                        .replay_collation(
                            slot,
                            created_at,
                            crate::storage::CollationDefinition {
                                schema,
                                name,
                                owner,
                                provider,
                                deterministic,
                                encoding,
                                collate,
                                ctype,
                                locale,
                                rules,
                                version,
                                behavior,
                            },
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest collation rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("conv") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let slot = parse_field(words.next(), "conversion slot")?;
                    let created_at = parse_field(words.next(), "conversion created_at")?;
                    let schema =
                        sql_name(&decode_hex_name(words.next().ok_or(
                            CheckpointSetupError::Corrupt("conversion schema missing"),
                        )?)?)?;
                    let name =
                        sql_name(&decode_hex_name(words.next().ok_or(
                            CheckpointSetupError::Corrupt("conversion name missing"),
                        )?)?)?;
                    let owner: i32 = parse_field(words.next(), "conversion owner")?;
                    if storage.role_slot_by_oid(owner, 0).is_none() {
                        return Err(CheckpointSetupError::Corrupt("conversion owner missing"));
                    }
                    let source = crate::storage::PgEncoding::from_code(parse_field(
                        words.next(),
                        "conversion source",
                    )?)
                    .ok_or(CheckpointSetupError::Corrupt("invalid conversion source"))?;
                    let destination = crate::storage::PgEncoding::from_code(parse_field(
                        words.next(),
                        "conversion destination",
                    )?)
                    .ok_or(CheckpointSetupError::Corrupt(
                        "invalid conversion destination",
                    ))?;
                    let procedure = parse_field(words.next(), "conversion procedure")?;
                    let default = match parse_field::<u8>(words.next(), "conversion default")? {
                        0 => false,
                        1 => true,
                        _ => {
                            return Err(CheckpointSetupError::Corrupt(
                                "invalid conversion default",
                            ));
                        }
                    };
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt("trailing conversion fields"));
                    }
                    storage
                        .replay_conversion(
                            slot,
                            created_at,
                            crate::storage::ConversionDefinition {
                                schema,
                                name,
                                owner,
                                source,
                                destination,
                                procedure,
                                default,
                            },
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest conversion rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("opr") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let identity = parse_catalog_position(&mut words, metadata.catalog_sequence)?;
                    let schema =
                        sql_name(&decode_hex_name(words.next().ok_or(
                            CheckpointSetupError::Corrupt("operator schema missing"),
                        )?)?)?;
                    let name =
                        sql_name(&decode_hex_name(words.next().ok_or(
                            CheckpointSetupError::Corrupt("operator name missing"),
                        )?)?)?;
                    let owner_oid = parse_field(words.next(), "operator owner")?;
                    if storage.role_slot_by_oid(owner_oid, 0).is_none() {
                        return Err(CheckpointSetupError::Corrupt("operator owner missing"));
                    }
                    let owner = owner_oid;
                    let signature_flags: u8 = parse_field(words.next(), "operator signature")?;
                    if signature_flags == 0 || signature_flags & !3 != 0 {
                        return Err(CheckpointSetupError::Corrupt("invalid operator signature"));
                    }
                    let left = manifest_routine_result(&mut words)?;
                    let right = manifest_routine_result(&mut words)?;
                    let result = manifest_routine_result(&mut words)?;
                    let function_oid = parse_field(words.next(), "operator function")?;
                    let commutator_oid: i32 = parse_field(words.next(), "operator commutator")?;
                    let negator_oid: i32 = parse_field(words.next(), "operator negator")?;
                    let hashes = parse_field::<u8>(words.next(), "operator hashes")?;
                    let merges = parse_field::<u8>(words.next(), "operator merges")?;
                    if hashes > 1 || merges > 1 || words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt("invalid operator fields"));
                    }
                    if function_oid < 0
                        || (function_oid != 0
                            && storage.routine_slot_by_oid(function_oid, 0).is_none())
                    {
                        return Err(CheckpointSetupError::Corrupt("operator function missing"));
                    }
                    let linked = |oid: i32| -> Result<Option<i32>, CheckpointSetupError> {
                        if oid == 0 {
                            Ok(None)
                        } else {
                            storage
                                .operator_slot_by_oid(oid, 0)
                                .map(|_| Some(oid))
                                .ok_or(CheckpointSetupError::Corrupt("linked operator missing"))
                        }
                    };
                    storage
                        .replay_set_operator(
                            identity,
                            crate::storage::OperatorDefinition {
                                schema,
                                name,
                                signature: crate::storage::OperatorSignature {
                                    left: (signature_flags & 1 != 0).then_some(left),
                                    right: (signature_flags & 2 != 0).then_some(right),
                                },
                                implementation: if function_oid == 0 {
                                    crate::storage::OperatorImplementation::Shell
                                } else {
                                    crate::storage::OperatorImplementation::Function {
                                        routine: function_oid,
                                        result,
                                    }
                                },
                                commutator: linked(commutator_oid)?,
                                negator: linked(negator_oid)?,
                                hashes: hashes != 0,
                                merges: merges != 0,
                                owner,
                            },
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest operator rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("oprl") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let operator_oid = parse_field(words.next(), "operator oid")?;
                    let commutator_oid: i32 = parse_field(words.next(), "operator commutator")?;
                    let negator_oid: i32 = parse_field(words.next(), "operator negator")?;
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt(
                            "trailing operator link fields",
                        ));
                    }
                    let slot = storage
                        .operator_slot_by_oid(operator_oid, 0)
                        .ok_or(CheckpointSetupError::Corrupt("operator missing"))?;
                    let linked = |oid: i32| -> Result<Option<i32>, CheckpointSetupError> {
                        if oid == 0 {
                            Ok(None)
                        } else {
                            storage
                                .operator_slot_by_oid(oid, 0)
                                .map(|_| Some(oid))
                                .ok_or(CheckpointSetupError::Corrupt("linked operator missing"))
                        }
                    };
                    let mut definition = storage.operator_for(slot, 0);
                    definition.commutator = linked(commutator_oid)?;
                    definition.negator = linked(negator_oid)?;
                    let created_at = storage.operator(slot).created_at;
                    storage
                        .replay_set_operator(
                            crate::storage::CatalogPosition::new(slot, created_at)
                                .expect("admitted executable catalog position"),
                            definition,
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest operator links rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("opf") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let identity = parse_catalog_position(&mut words, metadata.catalog_sequence)?;
                    let schema = sql_name(&decode_hex_name(words.next().ok_or(
                        CheckpointSetupError::Corrupt("operator family schema missing"),
                    )?)?)?;
                    let name = sql_name(&decode_hex_name(words.next().ok_or(
                        CheckpointSetupError::Corrupt("operator family name missing"),
                    )?)?)?;
                    let owner_oid = parse_field(words.next(), "operator family owner")?;
                    if storage.role_slot_by_oid(owner_oid, 0).is_none() {
                        return Err(CheckpointSetupError::Corrupt(
                            "operator family owner missing",
                        ));
                    }
                    let owner = owner_oid;
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt(
                            "trailing operator family fields",
                        ));
                    }
                    storage
                        .replay_set_operator_family(
                            identity,
                            crate::storage::OperatorFamilyDefinition {
                                schema,
                                name,
                                owner,
                                operators: [crate::storage::OperatorFamilyOperator::EMPTY;
                                    crate::storage::MAX_OPERATOR_FAMILY_MEMBERS],
                                functions: [crate::storage::OperatorFamilyFunction::EMPTY;
                                    crate::storage::MAX_OPERATOR_FAMILY_MEMBERS],
                            },
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest operator family rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("opfo") | Some("opff") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let is_operator = line.starts_with("opfo ");
                    let family_oid = parse_field(words.next(), "operator family oid")?;
                    let family_slot = storage
                        .operator_family_slot_by_oid(family_oid, 0)
                        .ok_or(CheckpointSetupError::Corrupt("operator family missing"))?;
                    let mut definition = storage.operator_family_for(family_slot, 0);
                    if is_operator {
                        let strategy = crate::sql::ast::BtreeStrategy::from_number(parse_field(
                            words.next(),
                            "operator family strategy",
                        )?)
                        .ok_or(CheckpointSetupError::Corrupt(
                            "invalid operator family strategy",
                        ))?;
                        let left = manifest_routine_result(&mut words)?;
                        let right = manifest_routine_result(&mut words)?;
                        let operator_oid = parse_field(words.next(), "operator family operator")?;
                        if storage.operator_contract(operator_oid, 0).is_none() {
                            return Err(CheckpointSetupError::Corrupt(
                                "operator family operator missing",
                            ));
                        }
                        let target = definition
                            .operators
                            .iter_mut()
                            .find(|member| !member.used)
                            .ok_or(CheckpointSetupError::Corrupt(
                                "too many operator family operators",
                            ))?;
                        *target = crate::storage::OperatorFamilyOperator {
                            used: true,
                            strategy,
                            left,
                            right,
                            operator: operator_oid,
                        };
                    } else {
                        let left = manifest_routine_result(&mut words)?;
                        let right = manifest_routine_result(&mut words)?;
                        let function_oid = parse_field(words.next(), "operator family function")?;
                        if storage.routine_slot_by_oid(function_oid, 0).is_none() {
                            return Err(CheckpointSetupError::Corrupt(
                                "operator family function missing",
                            ));
                        }
                        let target = definition
                            .functions
                            .iter_mut()
                            .find(|member| !member.used)
                            .ok_or(CheckpointSetupError::Corrupt(
                                "too many operator family functions",
                            ))?;
                        *target = crate::storage::OperatorFamilyFunction {
                            used: true,
                            left,
                            right,
                            function: function_oid,
                        };
                    }
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt(
                            "trailing operator family member fields",
                        ));
                    }
                    let created_at = storage.operator_family(family_slot).created_at;
                    storage
                        .replay_set_operator_family(
                            crate::storage::CatalogPosition::new(family_slot, created_at)
                                .expect("admitted executable catalog position"),
                            definition,
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest operator family member rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("opc") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let identity = parse_catalog_position(&mut words, metadata.catalog_sequence)?;
                    let schema = sql_name(&decode_hex_name(words.next().ok_or(
                        CheckpointSetupError::Corrupt("operator class schema missing"),
                    )?)?)?;
                    let name =
                        sql_name(&decode_hex_name(words.next().ok_or(
                            CheckpointSetupError::Corrupt("operator class name missing"),
                        )?)?)?;
                    let owner_oid = parse_field(words.next(), "operator class owner")?;
                    if storage.role_slot_by_oid(owner_oid, 0).is_none() {
                        return Err(CheckpointSetupError::Corrupt(
                            "operator class owner missing",
                        ));
                    }
                    let owner = owner_oid;
                    let family_oid = parse_field(words.next(), "operator class family")?;
                    if storage.operator_family_slot_by_oid(family_oid, 0).is_none() {
                        return Err(CheckpointSetupError::Corrupt(
                            "operator class family missing",
                        ));
                    }
                    let input = manifest_routine_result(&mut words)?;
                    let key_storage = manifest_routine_result(&mut words)?;
                    let default = match parse_field::<u8>(words.next(), "operator class default")? {
                        0 => false,
                        1 => true,
                        _ => {
                            return Err(CheckpointSetupError::Corrupt(
                                "invalid operator class default",
                            ));
                        }
                    };
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt(
                            "trailing operator class fields",
                        ));
                    }
                    storage
                        .replay_set_operator_class(
                            identity,
                            crate::storage::OperatorClassDefinition {
                                schema,
                                name,
                                owner,
                                family: family_oid,
                                input,
                                storage: key_storage,
                                default,
                                operators: [crate::storage::OperatorFamilyOperator::EMPTY;
                                    crate::storage::MAX_OPERATOR_FAMILY_MEMBERS],
                                functions: [crate::storage::OperatorFamilyFunction::EMPTY;
                                    crate::storage::MAX_OPERATOR_FAMILY_MEMBERS],
                            },
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest operator class rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                tag @ (Some("opco") | Some("opcf")) => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let class_oid: i32 = parse_field(words.next(), "operator class member class")?;
                    let class_oid = crate::storage::OperatorClassOid::parse(class_oid).ok_or(
                        CheckpointSetupError::Corrupt("invalid operator class identity"),
                    )?;
                    let class_slot = storage.operator_class_slot_by_oid(class_oid, 0).ok_or(
                        CheckpointSetupError::Corrupt("operator class member class missing"),
                    )?;
                    let mut definition = storage.operator_class_for(class_slot, 0);
                    if tag == Some("opco") {
                        let strategy = parse_field::<u32>(words.next(), "operator class strategy")
                            .ok()
                            .and_then(crate::sql::ast::BtreeStrategy::from_number)
                            .ok_or(CheckpointSetupError::Corrupt(
                                "invalid operator class strategy",
                            ))?;
                        let left = manifest_routine_result(&mut words)?;
                        let right = manifest_routine_result(&mut words)?;
                        let operator = parse_field(words.next(), "operator class operator")?;
                        if storage.operator_contract(operator, 0).is_none() {
                            return Err(CheckpointSetupError::Corrupt(
                                "operator class operator missing",
                            ));
                        }
                        let target = definition
                            .operators
                            .iter_mut()
                            .find(|member| !member.used)
                            .ok_or(CheckpointSetupError::Corrupt(
                                "too many operator class operators",
                            ))?;
                        *target = crate::storage::OperatorFamilyOperator {
                            used: true,
                            strategy,
                            left,
                            right,
                            operator,
                        };
                    } else {
                        let left = manifest_routine_result(&mut words)?;
                        let right = manifest_routine_result(&mut words)?;
                        let function = parse_field(words.next(), "operator class function")?;
                        if storage.routine_slot_by_oid(function, 0).is_none() {
                            return Err(CheckpointSetupError::Corrupt(
                                "operator class function missing",
                            ));
                        }
                        let target = definition
                            .functions
                            .iter_mut()
                            .find(|member| !member.used)
                            .ok_or(CheckpointSetupError::Corrupt(
                                "too many operator class functions",
                            ))?;
                        *target = crate::storage::OperatorFamilyFunction {
                            used: true,
                            left,
                            right,
                            function,
                        };
                    }
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt(
                            "trailing operator class member fields",
                        ));
                    }
                    let created_at = storage.operator_class(class_slot).created_at;
                    storage
                        .replay_set_operator_class(
                            crate::storage::CatalogPosition::new(class_slot, created_at)
                                .expect("admitted executable catalog position"),
                            definition,
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest operator class member rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("trg") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    load_trigger(storage, line)?;
                }
                Some("trgs") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    load_partition_trigger_state(storage, line)?;
                }
                Some("pol") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    load_policy(storage, line)?;
                }
                Some("sq7") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let read_hex = |w: Option<&str>, what: &'static str| {
                        w.ok_or(CheckpointSetupError::Corrupt(what))
                            .and_then(|value| {
                                if value == "-" {
                                    Ok(String::new())
                                } else {
                                    decode_hex_name(value)
                                }
                            })
                    };
                    let position = parse_catalog_position(&mut words, metadata.catalog_sequence)?;
                    let schema = read_hex(words.next(), "sequence schema missing")?;
                    let name = read_hex(words.next(), "sequence name missing")?;
                    let persistence = words
                        .next()
                        .filter(|word| word.len() == 1)
                        .and_then(|word| word.as_bytes().first().copied())
                        .and_then(crate::storage::RelationPersistence::from_code)
                        .ok_or(CheckpointSetupError::Corrupt(
                            "invalid sequence persistence",
                        ))?;
                    if persistence == crate::storage::RelationPersistence::Temporary {
                        return Err(CheckpointSetupError::Corrupt(
                            "temporary sequence in manifest",
                        ));
                    }
                    let data_type: u8 = parse_field(words.next(), "sequence type")?;
                    let increment: i64 = parse_field(words.next(), "sequence increment")?;
                    let min_value: i64 = parse_field(words.next(), "sequence min")?;
                    let max_value: i64 = parse_field(words.next(), "sequence max")?;
                    let start_value: i64 = parse_field(words.next(), "sequence start")?;
                    let cache: i64 = parse_field(words.next(), "sequence cache")?;
                    let cycle: u8 = parse_field(words.next(), "sequence cycle")?;
                    let last_value: i64 = parse_field(words.next(), "sequence last")?;
                    let is_called: u8 = parse_field(words.next(), "sequence is_called")?;
                    let log_count: i64 = parse_field(words.next(), "sequence log_count")?;
                    if cycle > 1 || is_called > 1 || log_count < 0 {
                        return Err(CheckpointSetupError::Corrupt(
                            "invalid sequence value image",
                        ));
                    }
                    let read_link = |words: &mut core::str::Split<'_, char>,
                                     label: &'static str|
                     -> Result<
                        Option<crate::storage::SequenceOwner>,
                        CheckpointSetupError,
                    > {
                        let read_owner = |word: Option<&str>, what: &'static str| match word
                            .ok_or(CheckpointSetupError::Corrupt(what))?
                        {
                            "0" => Ok(String::new()),
                            hex => decode_hex_name(hex),
                        };
                        let owner_schema = read_owner(words.next(), label)?;
                        let owner_table = read_owner(words.next(), label)?;
                        let owner_column = read_owner(words.next(), label)?;
                        if owner_schema.is_empty()
                            && owner_table.is_empty()
                            && owner_column.is_empty()
                        {
                            Ok(None)
                        } else if owner_schema.is_empty()
                            || owner_table.is_empty()
                            || owner_column.is_empty()
                        {
                            Err(CheckpointSetupError::Corrupt(
                                "incomplete sequence ownership link",
                            ))
                        } else {
                            Ok(Some(crate::storage::SequenceOwner {
                                table_schema: sql_name(&owner_schema)?,
                                table: sql_name(&owner_table)?,
                                column: sql_name(&owner_column)?,
                            }))
                        }
                    };
                    let owner = read_link(&mut words, "sequence owner missing")?;
                    let generator_for = read_link(&mut words, "sequence generator missing")?;
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt("trailing sequence fields"));
                    }
                    let slot = storage
                        .create_sequence(
                            crate::storage::SequenceCreateSpec {
                                allocation: crate::storage::CatalogAllocation::Restore(position),
                                schema: sql_name(&schema)?,
                                name: sql_name(&name)?,
                                spec: crate::storage::SeqSpec {
                                    data_type: crate::storage::SeqType::from_u8(data_type).ok_or(
                                        CheckpointSetupError::Corrupt("invalid sequence type"),
                                    )?,
                                    increment,
                                    min_value,
                                    max_value,
                                    start_value,
                                    cache,
                                    cycle: cycle != 0,
                                },
                                owner,
                                generator_for,
                                persistence,
                            },
                            0,
                        )
                        .map_err(|e| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest sequence rejected: {}",
                                e.message.as_str()
                            ))
                        })?;
                    storage
                        .finish_sequence_change(slot, crate::storage::CatalogDdlDecision::Commit);
                    storage.restore_sequence_value_image(
                        slot.slot(),
                        last_value,
                        is_called != 0,
                        log_count,
                    );
                }
                Some("dom6") => {
                    let source_slot: usize = parse_field(words.next(), "domain source slot")?;
                    if source_slot >= storage.domain_count() {
                        return Err(CheckpointSetupError::Corrupt(
                            "stored domain slot exceeds configured capacity",
                        ));
                    }
                    let created_at: u64 = parse_field(words.next(), "domain source lifetime")?;
                    if created_at == 0 || created_at > metadata.catalog_sequence {
                        return Err(CheckpointSetupError::Corrupt(
                            "invalid domain source lifetime",
                        ));
                    }
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    // A `0` field is the empty-string sentinel; anything else is
                    // even-length hex.
                    let hexstr = |w: Option<&str>,
                                  what: &'static str|
                     -> Result<String, CheckpointSetupError> {
                        match w.ok_or(CheckpointSetupError::Corrupt(what))? {
                            "0" => Ok(String::new()),
                            h => decode_hex_name(h),
                        }
                    };
                    let base_code: u8 = parse_field(words.next(), "dom base")?;
                    let base_type_mod: i32 = parse_field(words.next(), "dom typmod")?;
                    let not_null: u8 = parse_field(words.next(), "dom notnull")?;
                    if not_null > 1 {
                        return Err(CheckpointSetupError::Corrupt(
                            "invalid domain NOT NULL flag",
                        ));
                    }
                    let n_checks: usize = parse_field(words.next(), "dom nchecks")?;
                    if n_checks > crate::storage::MAX_DOMAIN_CHECKS {
                        return Err(CheckpointSetupError::Corrupt("too many domain checks"));
                    }
                    let schema = hexstr(words.next(), "dom schema missing")?;
                    let name = hexstr(words.next(), "dom name missing")?;
                    let base_domain = hexstr(words.next(), "dom base domain missing")?;
                    let base_domain_schema =
                        hexstr(words.next(), "dom base domain schema missing")?;
                    let base_user_type = hexstr(words.next(), "dom base type missing")?;
                    let base_user_type_schema =
                        hexstr(words.next(), "dom base type schema missing")?;
                    let not_null_text = hexstr(words.next(), "dom NOT NULL name missing")?;
                    let not_null_name = if not_null_text.is_empty() {
                        None
                    } else {
                        Some(sql_name(&not_null_text)?)
                    };
                    if (not_null != 0) != not_null_name.is_some() {
                        return Err(CheckpointSetupError::Corrupt(
                            "invalid domain NOT NULL name",
                        ));
                    }
                    let default_text = hexstr(words.next(), "dom default missing")?;
                    let base = crate::sql::types::ColType::from_code(base_code)
                        .ok_or(CheckpointSetupError::Corrupt("bad domain base type"))?;
                    let mut checks =
                        [crate::storage::CheckConstraint::EMPTY; crate::storage::MAX_DOMAIN_CHECKS];
                    for check in checks.iter_mut().take(n_checks) {
                        let cname = hexstr(words.next(), "dom check name missing")?;
                        let cexpr = hexstr(words.next(), "dom check expr missing")?;
                        let code: u8 = parse_field(words.next(), "dom check validation")?;
                        let validation = crate::storage::ConstraintValidation::from_code(code)
                            .ok_or(CheckpointSetupError::Corrupt("bad domain check validation"))?;
                        *check = crate::storage::CheckConstraint {
                            identity: None,
                            name: sql_name(&cname)?,
                            expression: crate::util::StackStr::from_str(&cexpr),
                            validation,
                        };
                    }
                    let base_domain = match (base_domain.is_empty(), base_domain_schema.is_empty())
                    {
                        (true, true) => None,
                        (false, false) => Some(
                            crate::storage::UserTypeName::from_storage(
                                sql_name(&base_domain_schema)?,
                                &base_domain,
                            )
                            .map_err(|_| {
                                CheckpointSetupError::Corrupt("invalid stored type identity")
                            })?,
                        ),
                        _ => {
                            return Err(CheckpointSetupError::Corrupt(
                                "domain parent identity is incomplete",
                            ));
                        }
                    };
                    let base_user_type =
                        match (base_user_type.is_empty(), base_user_type_schema.is_empty()) {
                            (true, true) => None,
                            (false, false) => Some(
                                crate::storage::UserTypeName::from_storage(
                                    sql_name(&base_user_type_schema)?,
                                    &base_user_type,
                                )
                                .map_err(|_| {
                                    CheckpointSetupError::Corrupt("invalid stored type identity")
                                })?,
                            ),
                            _ => {
                                return Err(CheckpointSetupError::Corrupt(
                                    "domain base type identity is incomplete",
                                ));
                            }
                        };
                    for check in checks.iter_mut().take(n_checks) {
                        let created_at: u64 = parse_field(words.next(), "domain CHECK identity")?;
                        check.identity = Some(
                            crate::storage::ConstraintIdentity::parse(created_at)
                                .filter(|_| created_at <= metadata.catalog_sequence)
                                .ok_or(CheckpointSetupError::Corrupt(
                                    "invalid domain CHECK identity",
                                ))?,
                        );
                    }
                    let not_null_identity = if not_null != 0 {
                        let created_at: u64 =
                            parse_field(words.next(), "domain NOT NULL identity")?;
                        Some(
                            crate::storage::ConstraintIdentity::parse(created_at)
                                .filter(|_| created_at <= metadata.catalog_sequence)
                                .ok_or(CheckpointSetupError::Corrupt(
                                    "invalid domain NOT NULL identity",
                                ))?,
                        )
                    } else {
                        None
                    };
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt("unexpected domain fields"));
                    }
                    let spec = crate::storage::DomainSpec {
                        base_domain,
                        base_user_type,
                        base,
                        base_type_mod,
                        not_null: not_null != 0,
                        not_null_name,
                        not_null_identity,
                        default_expr: (!default_text.is_empty())
                            .then(|| crate::util::StackStr::from_str(&default_text)),
                        checks,
                        n_checks,
                    };
                    let allocated = storage
                        .create_domain_from_manifest(
                            sql_name(&schema)?,
                            sql_name(&name)?,
                            spec,
                            created_at,
                        )
                        .map_err(|e| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest domain rejected: {}",
                                e.message.as_str()
                            ))
                        })?;
                    storage
                        .restore_type_identity(
                            crate::storage::NamedTypeSlot::Domain(allocated),
                            crate::storage::NamedTypeSlot::Domain(source_slot),
                            created_at,
                        )
                        .map_err(|_| {
                            CheckpointSetupError::Corrupt("invalid restored domain identity")
                        })?;
                }
                Some("enm3") => {
                    let source_slot: usize = parse_field(words.next(), "enm source slot")?;
                    if source_slot >= storage.enum_count() {
                        return Err(CheckpointSetupError::Corrupt(
                            "stored enm slot exceeds configured capacity",
                        ));
                    }
                    let created_at: u64 = parse_field(words.next(), "enm source lifetime")?;
                    if created_at == 0 || created_at > metadata.catalog_sequence {
                        return Err(CheckpointSetupError::Corrupt("invalid enm source lifetime"));
                    }
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let hexstr = |w: Option<&str>,
                                  what: &'static str|
                     -> Result<String, CheckpointSetupError> {
                        match w.ok_or(CheckpointSetupError::Corrupt(what))? {
                            "0" => Ok(String::new()),
                            h => decode_hex_name(h),
                        }
                    };
                    let schema = hexstr(words.next(), "enm schema missing")?;
                    let name = hexstr(words.next(), "enm name missing")?;
                    let n_members: usize = parse_field(words.next(), "enm nmembers")?;
                    if n_members > storage.enum_label_capacity() {
                        return Err(CheckpointSetupError::Corrupt("too many enum labels"));
                    }
                    let mut members = storage.enum_replay_members(n_members).map_err(|error| {
                        CheckpointSetupError::ObjectStore(error.message.as_str().to_string())
                    })?;
                    for member in members.iter_mut() {
                        let label = hexstr(words.next(), "enm label missing")?;
                        let sort_bits: u64 = parse_field(words.next(), "enm sort")?;
                        let label = sql_name(&label)?;
                        let legacy_label = match words.next() {
                            Some("-") => None,
                            Some(encoded) => Some(sql_name(&hexstr(
                                Some(encoded),
                                "enm legacy label missing",
                            )?)?),
                            None => {
                                return Err(CheckpointSetupError::Corrupt(
                                    "enm legacy label missing",
                                ));
                            }
                        };
                        let label_creation: u64 = parse_field(words.next(), "enm label creation")?;
                        let identity = crate::storage::EnumLabelIdentity::parse(label_creation)
                            .ok_or(CheckpointSetupError::Corrupt("invalid enum label creation"))?;
                        *member = crate::storage::EnumMember {
                            identity: Some(identity),
                            label,
                            legacy_label,
                            sort: f64::from_bits(sort_bits),
                        };
                    }
                    drop(members);
                    let allocated = storage
                        .finish_enum_replay(
                            sql_name(&schema)?,
                            sql_name(&name)?,
                            n_members,
                            created_at,
                        )
                        .map_err(|e| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest enum rejected: {}",
                                e.message.as_str()
                            ))
                        })?;
                    storage
                        .restore_type_identity(
                            crate::storage::NamedTypeSlot::Enum(allocated),
                            crate::storage::NamedTypeSlot::Enum(source_slot),
                            created_at,
                        )
                        .map_err(|_| {
                            CheckpointSetupError::Corrupt("invalid restored enm identity")
                        })?;
                }
                Some("cmp2") => {
                    let source_slot: usize = parse_field(words.next(), "cmp source slot")?;
                    if source_slot >= storage.composite_count() {
                        return Err(CheckpointSetupError::Corrupt(
                            "stored cmp slot exceeds configured capacity",
                        ));
                    }
                    let created_at: u64 = parse_field(words.next(), "cmp source lifetime")?;
                    if created_at == 0 || created_at > metadata.catalog_sequence {
                        return Err(CheckpointSetupError::Corrupt("invalid cmp source lifetime"));
                    }
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let hexstr = |w: Option<&str>,
                                  what: &'static str|
                     -> Result<String, CheckpointSetupError> {
                        match w.ok_or(CheckpointSetupError::Corrupt(what))? {
                            "0" => Ok(String::new()),
                            h => decode_hex_name(h),
                        }
                    };
                    let schema = hexstr(words.next(), "cmp schema missing")?;
                    let name = hexstr(words.next(), "cmp name missing")?;
                    let n_fields: usize = parse_field(words.next(), "cmp nfields")?;
                    if n_fields > crate::storage::MAX_COMPOSITE_FIELDS {
                        return Err(CheckpointSetupError::Corrupt("too many composite fields"));
                    }
                    let mut fields = [crate::storage::CompositeFieldDef::EMPTY;
                        crate::storage::MAX_COMPOSITE_FIELDS];
                    for field in fields.iter_mut().take(n_fields) {
                        let field_name = hexstr(words.next(), "cmp field name missing")?;
                        let attribute_number: u16 =
                            parse_field(words.next(), "cmp attribute number missing")?;
                        if attribute_number == 0 {
                            return Err(CheckpointSetupError::Corrupt(
                                "zero composite attribute number",
                            ));
                        }
                        let dropped: u8 =
                            parse_field(words.next(), "cmp attribute dropped missing")?;
                        let dropped = match dropped {
                            0 => false,
                            1 => true,
                            _ => {
                                return Err(CheckpointSetupError::Corrupt(
                                    "bad composite attribute dropped",
                                ));
                            }
                        };
                        let not_null: u8 =
                            parse_field(words.next(), "cmp attribute not-null missing")?;
                        let not_null = match not_null {
                            0 => false,
                            1 => true,
                            _ => {
                                return Err(CheckpointSetupError::Corrupt(
                                    "bad composite attribute not-null",
                                ));
                            }
                        };
                        let code: u8 = parse_field(words.next(), "cmp field type missing")?;
                        let type_mod: i32 = parse_field(words.next(), "cmp field typmod missing")?;
                        let collation_code: u32 =
                            parse_field(words.next(), "cmp field collation missing")?;
                        let collation = crate::sql::ast::Collation::from_code(collation_code)
                            .ok_or(CheckpointSetupError::Corrupt(
                                "bad composite field collation",
                            ))?;
                        let user_schema = hexstr(words.next(), "cmp field user schema missing")?;
                        let user_name = hexstr(words.next(), "cmp field user name missing")?;
                        let user_type = match (user_schema.is_empty(), user_name.is_empty()) {
                            (true, true) => None,
                            (false, false) => Some(
                                crate::storage::UserTypeName::from_storage(
                                    sql_name(&user_schema)?,
                                    &user_name,
                                )
                                .map_err(|_| {
                                    CheckpointSetupError::Corrupt("invalid stored type identity")
                                })?,
                            ),
                            _ => {
                                return Err(CheckpointSetupError::Corrupt(
                                    "composite field user type identity is incomplete",
                                ));
                            }
                        };
                        *field = crate::storage::CompositeFieldDef {
                            attribute_number,
                            name: sql_name(&field_name)?,
                            ctype: crate::sql::types::ColType::from_code(code)
                                .ok_or(CheckpointSetupError::Corrupt("bad composite field type"))?,
                            type_mod,
                            collation,
                            user_type,
                            dropped,
                            not_null,
                        };
                    }
                    let allocated = storage
                        .create_composite_from_manifest(
                            sql_name(&schema)?,
                            sql_name(&name)?,
                            crate::storage::CompositeSpec { fields, n_fields },
                            created_at,
                        )
                        .map_err(|e| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest composite rejected: {}",
                                e.message.as_str()
                            ))
                        })?;
                    storage
                        .restore_type_identity(
                            crate::storage::NamedTypeSlot::Composite(allocated),
                            crate::storage::NamedTypeSlot::Composite(source_slot),
                            created_at,
                        )
                        .map_err(|_| {
                            CheckpointSetupError::Corrupt("invalid restored cmp identity")
                        })?;
                }
                Some("ext") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let created_at: u64 = parse_field(words.next(), "ext sequence")?;
                    let owner: usize = parse_field(words.next(), "ext owner")?;
                    let relocatable = match parse_field::<u8>(words.next(), "ext relocatable")? {
                        0 => false,
                        1 => true,
                        _ => return Err(CheckpointSetupError::Corrupt("ext relocatable")),
                    };
                    let name = sql_name(&decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("ext name"))?,
                    )?)?;
                    let schema = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("ext schema"))?,
                    )?;
                    let version = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("ext version"))?,
                    )?;
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt("trailing ext fields"));
                    }
                    let namespace = storage
                        .find_schema(&schema)
                        .ok_or(CheckpointSetupError::Corrupt("ext schema does not exist"))?;
                    if owner >= storage.role_count() || !storage.role(owner).visible_to(0) {
                        return Err(CheckpointSetupError::Corrupt("ext owner does not exist"));
                    }
                    storage
                        .install_extension(
                            name,
                            namespace,
                            relocatable,
                            crate::storage::ExtensionVersion::parse(&version).map_err(|_| {
                                CheckpointSetupError::Corrupt("ext version invalid")
                            })?,
                            owner,
                            created_at,
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest extension rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("xpk") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let package_slot: usize = parse_field(words.next(), "extension package slot")?;
                    let key = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("extension package key"))?;
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt(
                            "trailing extension package fields",
                        ));
                    }
                    if storage.extension_package_source()
                        == crate::storage::ExtensionPackageSource::Configured
                    {
                        continue;
                    }
                    if package_slot != storage.extension_packages().count() {
                        return Err(CheckpointSetupError::Corrupt(
                            "extension package slots are not ordered",
                        ));
                    }
                    self.client.get(key, None).map_err(|error| {
                        CheckpointSetupError::ObjectStore(format!(
                            "load durable extension package: {error}"
                        ))
                    })?;
                    verify_extension_object_key(
                        key,
                        "extensions/meta/",
                        ".pkg",
                        self.client.body_bytes(),
                    )?;
                    let package = decode_extension_package(self.client.body_bytes())?;
                    storage
                        .install_durable_extension_package(package)
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "durable extension package rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("xsc") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let package_slot: usize =
                        parse_field(words.next(), "extension script package")?;
                    let from = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("extension script from"))?,
                    )?;
                    let to = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("extension script to"))?,
                    )?;
                    let metadata_key = words.next().ok_or(CheckpointSetupError::Corrupt(
                        "extension script metadata key",
                    ))?;
                    let source_key = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("extension script source key"))?;
                    let expected_length: usize =
                        parse_field(words.next(), "extension script length")?;
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt(
                            "trailing extension script fields",
                        ));
                    }
                    if storage.extension_package_source()
                        == crate::storage::ExtensionPackageSource::Configured
                    {
                        continue;
                    }
                    self.client.get(metadata_key, None).map_err(|error| {
                        CheckpointSetupError::ObjectStore(format!(
                            "load durable extension version metadata: {error}"
                        ))
                    })?;
                    verify_extension_object_key(
                        metadata_key,
                        "extensions/meta/",
                        ".pkg",
                        self.client.body_bytes(),
                    )?;
                    let effective = decode_extension_package(self.client.body_bytes())?;
                    self.client.get(source_key, None).map_err(|error| {
                        CheckpointSetupError::ObjectStore(format!(
                            "load durable extension script: {error}"
                        ))
                    })?;
                    verify_extension_object_key(
                        source_key,
                        "extensions/sql/",
                        ".sql",
                        self.client.body_bytes(),
                    )?;
                    if self.client.body_bytes().len() != expected_length
                        || core::str::from_utf8(self.client.body_bytes()).is_err()
                    {
                        return Err(CheckpointSetupError::Corrupt(
                            "invalid durable extension script",
                        ));
                    }
                    let from = if from.is_empty() {
                        None
                    } else {
                        Some(crate::storage::ExtensionVersion::parse(&from).map_err(|_| {
                            CheckpointSetupError::Corrupt("invalid extension script source version")
                        })?)
                    };
                    let to = crate::storage::ExtensionVersion::parse(&to).map_err(|_| {
                        CheckpointSetupError::Corrupt("invalid extension script target version")
                    })?;
                    storage
                        .install_durable_extension_script(
                            package_slot,
                            from,
                            to,
                            effective,
                            self.client.body_bytes(),
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "durable extension script rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("exd") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let extension_name = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("exd extension"))?,
                    )?;
                    let class = crate::storage::AccessClass::from_u8(parse_field(
                        words.next(),
                        "exd class",
                    )?)
                    .ok_or(CheckpointSetupError::Corrupt("exd class invalid"))?;
                    let object_oid: i32 = parse_field(words.next(), "exd object oid")?;
                    let decode_identity = |value: Option<&str>, missing| {
                        let value = value.ok_or(CheckpointSetupError::Corrupt(missing))?;
                        if value == "-" {
                            Ok(String::new())
                        } else {
                            decode_hex_name(value)
                        }
                    };
                    let schema = decode_identity(words.next(), "exd schema")?;
                    let name = decode_identity(words.next(), "exd name")?;
                    let kind = match parse_field::<u8>(words.next(), "exd kind")? {
                        0 => crate::storage::ExtensionDependencyKind::Member,
                        1 => crate::storage::ExtensionDependencyKind::Automatic,
                        2 => crate::storage::ExtensionDependencyKind::Required,
                        _ => return Err(CheckpointSetupError::Corrupt("exd kind invalid")),
                    };
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt("trailing exd fields"));
                    }
                    let extension = storage.extension_slot(&extension_name, 0).ok_or(
                        CheckpointSetupError::Corrupt("exd extension does not exist"),
                    )?;
                    let object = if class == crate::storage::AccessClass::Routine {
                        storage.routine_slot_by_oid(object_oid, 0).map(|slot| {
                            crate::storage::AccessObject {
                                class,
                                slot: slot as u16,
                            }
                        })
                    } else {
                        storage.resolve_access_object(class, &schema, &name, 0)
                    }
                    .ok_or(CheckpointSetupError::Corrupt("exd object does not exist"))?;
                    let (slot, _) = storage
                        .change_extension_dependency(extension, object, kind, true, 0)
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest extension dependency rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                    storage.commit_extension_dependency(slot, 0);
                }
                Some("exc") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let extension_name = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("exc extension"))?,
                    )?;
                    let ordinal = parse_field(words.next(), "exc ordinal")?;
                    let relation_kind = crate::storage::ExtensionConfigRelationKind::from_u8(
                        parse_field(words.next(), "exc relation kind")?,
                    )
                    .ok_or(CheckpointSetupError::Corrupt("exc relation kind invalid"))?;
                    let schema = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("exc schema"))?,
                    )?;
                    let name = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("exc name"))?,
                    )?;
                    let condition = match words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("exc condition"))?
                    {
                        "-" => String::new(),
                        encoded => decode_hex_name(encoded)?,
                    };
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt("trailing exc fields"));
                    }
                    let extension = storage.extension_slot(&extension_name, 0).ok_or(
                        CheckpointSetupError::Corrupt("exc extension does not exist"),
                    )?;
                    let object = match relation_kind {
                        crate::storage::ExtensionConfigRelationKind::Table => storage
                            .resolve_access_object(
                                crate::storage::AccessClass::Table,
                                &schema,
                                &name,
                                0,
                            ),
                        crate::storage::ExtensionConfigRelationKind::Sequence => storage
                            .resolve_access_object(
                                crate::storage::AccessClass::Sequence,
                                &schema,
                                &name,
                                0,
                            ),
                    }
                    .ok_or(CheckpointSetupError::Corrupt("exc relation does not exist"))?;
                    let relation =
                        crate::storage::ExtensionConfigRelation::from_access_object(object)
                            .ok_or(CheckpointSetupError::Corrupt("exc relation class invalid"))?;
                    let condition = crate::storage::extension_config_condition(&condition)
                        .map_err(|_| CheckpointSetupError::Corrupt("exc condition too long"))?;
                    let (slot, _) = storage
                        .replay_extension_config(extension, relation, condition, true, ordinal)
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest extension configuration rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                    storage.commit_extension_config(slot, 0);
                }
                Some("cmt") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let class: u8 = parse_field(words.next(), "cmt class")?;
                    let subid: u32 = parse_field(words.next(), "cmt subid")?;
                    let read_hex = |w: Option<&str>, what: &'static str| {
                        w.ok_or(CheckpointSetupError::Corrupt(what))
                            .and_then(decode_hex_name)
                    };
                    let schema = read_hex(words.next(), "cmt schema missing")?;
                    let name = read_hex(words.next(), "cmt name missing")?;
                    let text = read_hex(words.next(), "cmt text missing")?;
                    let stored = crate::storage::comment_stackstr(&text)
                        .map_err(|_| CheckpointSetupError::Corrupt("cmt text too long"))?;
                    let class = crate::storage::CommentClass::from_u8(class)
                        .ok_or(CheckpointSetupError::Corrupt("cmt class unknown"))?;
                    storage
                        .apply_comment(
                            class,
                            sql_name(&schema)?,
                            sql_name(&name)?,
                            subid,
                            Some(stored),
                        )
                        .map_err(|e| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest comment rejected: {}",
                                e.message.as_str()
                            ))
                        })?;
                }
                Some("tsp") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let mut words = line.split_ascii_whitespace();
                    if words.next() != Some("tsp") {
                        return Err(CheckpointSetupError::Corrupt("tablespace tag"));
                    }
                    let slot: usize = parse_field(words.next(), "tablespace slot")?;
                    let created_at: u64 = parse_field(words.next(), "tablespace sequence")?;
                    let owner: u16 = parse_field(words.next(), "tablespace owner")?;
                    let name = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("tablespace name missing"))?,
                    )?;
                    let location =
                        decode_hex_name(words.next().ok_or(CheckpointSetupError::Corrupt(
                            "tablespace location missing",
                        ))?)?;
                    let random: u64 = parse_field(words.next(), "tablespace random cost")?;
                    let sequential: u64 = parse_field(words.next(), "tablespace seq cost")?;
                    let effective: i32 = parse_field(words.next(), "tablespace effective io")?;
                    let maintenance: i32 = parse_field(words.next(), "tablespace maintenance io")?;
                    if words.next().is_some() || created_at == 0 {
                        return Err(CheckpointSetupError::Corrupt("invalid tablespace record"));
                    }
                    let location = crate::util::StackStr::from_str(&location);
                    if location.is_truncated() {
                        return Err(CheckpointSetupError::Corrupt(
                            "tablespace location too long",
                        ));
                    }
                    let cost = |bits| {
                        if bits == u64::MAX {
                            Ok(None)
                        } else {
                            crate::sql::ast::TablespaceCost::from_bits(bits)
                                .map(Some)
                                .ok_or(CheckpointSetupError::Corrupt("invalid tablespace cost"))
                        }
                    };
                    storage
                        .restore_tablespace(
                            slot,
                            created_at,
                            sql_name(&name)?,
                            location,
                            crate::storage::TablespaceOptions {
                                random_page_cost: cost(random)?,
                                seq_page_cost: cost(sequential)?,
                                effective_io_concurrency: (effective != i32::MIN)
                                    .then_some(effective),
                                maintenance_io_concurrency: (maintenance != i32::MIN)
                                    .then_some(maintenance),
                            },
                            owner,
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest tablespace rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("idx") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    let mut words = line.split_ascii_whitespace();
                    if words.next() != Some("idx") {
                        return Err(CheckpointSetupError::Corrupt("idx tag"));
                    }
                    let created_at: u64 = parse_field(words.next(), "idx catalog sequence")?;
                    if created_at == 0 {
                        return Err(CheckpointSetupError::Corrupt("bad index catalog sequence"));
                    }
                    let unique: u8 = parse_field(words.next(), "idx unique")?;
                    let n_cols: usize = parse_field(words.next(), "idx ncols")?;
                    if n_cols == 0 || n_cols > crate::storage::MAX_INDEX_COLS {
                        return Err(CheckpointSetupError::Corrupt("bad index ncols"));
                    }
                    let mut columns = [0u16; crate::storage::MAX_INDEX_COLS];
                    for c in columns.iter_mut().take(n_cols) {
                        *c = parse_field(words.next(), "idx col")?;
                    }
                    let hex_name = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("idx name missing"))?;
                    let htable = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("idx table missing"))?;
                    let name = decode_hex_name(hex_name)?;
                    let table = decode_hex_name(htable)?;
                    let schema = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("idx schema missing"))?,
                    )?;
                    let descending_mask: u32 = parse_field(words.next(), "idx descending mask")?;
                    let nulls_first_mask: u32 = parse_field(words.next(), "idx nulls-first mask")?;
                    let predicate = match words.next() {
                        Some("-") => None,
                        Some(hex) => Some(
                            crate::storage::index_predicate_stackstr(&decode_hex_name(hex)?)
                                .map_err(|_| {
                                    CheckpointSetupError::Corrupt("idx predicate too long")
                                })?,
                        ),
                        None => {
                            return Err(CheckpointSetupError::Corrupt("idx predicate missing"));
                        }
                    };
                    let n_include_cols: usize =
                        parse_field(words.next(), "idx included column count")?;
                    if n_include_cols > crate::storage::MAX_INDEX_COLS {
                        return Err(CheckpointSetupError::Corrupt(
                            "bad index included column count",
                        ));
                    }
                    let mut include_columns = [0u16; crate::storage::MAX_INDEX_COLS];
                    for column in include_columns.iter_mut().take(n_include_cols) {
                        *column = parse_field(words.next(), "idx included column")?;
                    }
                    let nulls_not_distinct =
                        match parse_field(words.next(), "idx nulls-not-distinct")? {
                            0 => false,
                            1 => true,
                            _ => {
                                return Err(CheckpointSetupError::Corrupt(
                                    "bad index nulls-not-distinct",
                                ));
                            }
                        };
                    let mut expressions = [None; crate::storage::MAX_INDEX_COLS];
                    let mask: u32 = parse_field(words.next(), "idx expression mask")?;
                    if n_cols < u32::BITS as usize && mask >> n_cols != 0 {
                        return Err(CheckpointSetupError::Corrupt("bad index expression mask"));
                    };
                    for (index, expression) in expressions.iter_mut().enumerate().take(n_cols) {
                        if mask & (1u32 << index) != 0 {
                            *expression = Some(
                                crate::storage::index_expression_stackstr(&decode_hex_name(
                                    words.next().ok_or(CheckpointSetupError::Corrupt(
                                        "idx expression missing",
                                    ))?,
                                )?)
                                .map_err(|_| {
                                    CheckpointSetupError::Corrupt("idx expression too long")
                                })?,
                            );
                        }
                    }
                    let collation_count: usize = parse_field(words.next(), "idx collation count")?;
                    if collation_count != n_cols {
                        return Err(CheckpointSetupError::Corrupt("bad index collation count"));
                    }
                    let mut collations =
                        [crate::sql::ast::Collation::Default; crate::storage::MAX_INDEX_COLS];
                    let mut explicit_collations = [false; crate::storage::MAX_INDEX_COLS];
                    for position in 0..n_cols {
                        let code: u32 = parse_field(words.next(), "idx collation")?;
                        explicit_collations[position] =
                            match parse_field::<u8>(words.next(), "idx explicit collation")? {
                                0 => false,
                                1 => true,
                                _ => {
                                    return Err(CheckpointSetupError::Corrupt(
                                        "bad index explicit collation",
                                    ));
                                }
                            };
                        collations[position] = crate::sql::ast::Collation::from_code(code)
                            .ok_or(CheckpointSetupError::Corrupt("bad index collation"))?;
                    }
                    let mut operator_classes = [None; crate::storage::MAX_INDEX_COLS];
                    for operator_class in operator_classes.iter_mut().take(n_cols) {
                        let encoded = words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("idx operator class missing"))?;
                        *operator_class = if encoded == "0" {
                            None
                        } else if let Some(code) = encoded.strip_prefix('b') {
                            let code = code.parse().map_err(|_| {
                                CheckpointSetupError::Corrupt("bad builtin index operator class")
                            })?;
                            Some(crate::storage::IndexOperatorClass::Btree(
                                crate::sql::types::BtreeOperatorClass::from_code(code).ok_or(
                                    CheckpointSetupError::Corrupt(
                                        "bad builtin index operator class",
                                    ),
                                )?,
                            ))
                        } else if let Some(oid) = encoded.strip_prefix('c') {
                            let oid = oid.parse().map_err(|_| {
                                CheckpointSetupError::Corrupt("bad catalog index operator class")
                            })?;
                            Some(crate::storage::IndexOperatorClass::Catalog(
                                crate::storage::OperatorClassOid::parse(oid).ok_or(
                                    CheckpointSetupError::Corrupt(
                                        "bad catalog index operator class",
                                    ),
                                )?,
                            ))
                        } else if let Some(code) = encoded.strip_prefix('h') {
                            let code = code.parse().map_err(|_| {
                                CheckpointSetupError::Corrupt("bad hash index operator class")
                            })?;
                            Some(crate::storage::IndexOperatorClass::Hash(
                                crate::sql::types::HashOperatorClass::from_code(code).ok_or(
                                    CheckpointSetupError::Corrupt("bad hash index operator class"),
                                )?,
                            ))
                        } else if let Some(code) = encoded.strip_prefix('r') {
                            let code = code.parse().map_err(|_| {
                                CheckpointSetupError::Corrupt("bad brin index operator class")
                            })?;
                            Some(crate::storage::IndexOperatorClass::Brin(
                                crate::sql::types::BrinOperatorClass::from_code(code).ok_or(
                                    CheckpointSetupError::Corrupt("bad brin index operator class"),
                                )?,
                            ))
                        } else if let Some(code) = encoded.strip_prefix('g') {
                            let code = code.parse().map_err(|_| {
                                CheckpointSetupError::Corrupt("bad gist index operator class")
                            })?;
                            Some(crate::storage::IndexOperatorClass::Gist(
                                crate::sql::types::GistOperatorClass::from_code(code).ok_or(
                                    CheckpointSetupError::Corrupt("bad gist index operator class"),
                                )?,
                            ))
                        } else if let Some(code) = encoded.strip_prefix('n') {
                            let code = code.parse().map_err(|_| {
                                CheckpointSetupError::Corrupt("bad gin index operator class")
                            })?;
                            Some(crate::storage::IndexOperatorClass::Gin(
                                crate::sql::types::GinOperatorClass::from_code(code).ok_or(
                                    CheckpointSetupError::Corrupt("bad gin index operator class"),
                                )?,
                            ))
                        } else if let Some(code) = encoded.strip_prefix('s') {
                            let code = code.parse().map_err(|_| {
                                CheckpointSetupError::Corrupt("bad spgist index operator class")
                            })?;
                            Some(crate::storage::IndexOperatorClass::SpGist(
                                crate::sql::types::SpGistOperatorClass::from_code(code).ok_or(
                                    CheckpointSetupError::Corrupt(
                                        "bad spgist index operator class",
                                    ),
                                )?,
                            ))
                        } else {
                            return Err(CheckpointSetupError::Corrupt(
                                "bad index operator class encoding",
                            ));
                        };
                    }
                    let mut resolved_operator_classes = [None; crate::storage::MAX_INDEX_COLS];
                    for operator_class in resolved_operator_classes.iter_mut().take(n_cols) {
                        let encoded = words.next().ok_or(CheckpointSetupError::Corrupt(
                            "idx resolved operator class missing",
                        ))?;
                        *operator_class = if let Some(code) = encoded.strip_prefix('b') {
                            let code = code.parse().map_err(|_| {
                                CheckpointSetupError::Corrupt(
                                    "bad builtin resolved index operator class",
                                )
                            })?;
                            Some(crate::storage::IndexOperatorClass::Btree(
                                crate::sql::types::BtreeOperatorClass::from_code(code).ok_or(
                                    CheckpointSetupError::Corrupt(
                                        "bad builtin resolved index operator class",
                                    ),
                                )?,
                            ))
                        } else if let Some(oid) = encoded.strip_prefix('c') {
                            let oid = oid.parse().map_err(|_| {
                                CheckpointSetupError::Corrupt(
                                    "bad catalog resolved index operator class",
                                )
                            })?;
                            Some(crate::storage::IndexOperatorClass::Catalog(
                                crate::storage::OperatorClassOid::parse(oid).ok_or(
                                    CheckpointSetupError::Corrupt(
                                        "bad catalog resolved index operator class",
                                    ),
                                )?,
                            ))
                        } else if let Some(code) = encoded.strip_prefix('h') {
                            let code = code.parse().map_err(|_| {
                                CheckpointSetupError::Corrupt(
                                    "bad hash resolved index operator class",
                                )
                            })?;
                            Some(crate::storage::IndexOperatorClass::Hash(
                                crate::sql::types::HashOperatorClass::from_code(code).ok_or(
                                    CheckpointSetupError::Corrupt(
                                        "bad hash resolved index operator class",
                                    ),
                                )?,
                            ))
                        } else if let Some(code) = encoded.strip_prefix('r') {
                            let code = code.parse().map_err(|_| {
                                CheckpointSetupError::Corrupt(
                                    "bad brin resolved index operator class",
                                )
                            })?;
                            Some(crate::storage::IndexOperatorClass::Brin(
                                crate::sql::types::BrinOperatorClass::from_code(code).ok_or(
                                    CheckpointSetupError::Corrupt(
                                        "bad brin resolved index operator class",
                                    ),
                                )?,
                            ))
                        } else if let Some(code) = encoded.strip_prefix('g') {
                            let code = code.parse().map_err(|_| {
                                CheckpointSetupError::Corrupt(
                                    "bad gist resolved index operator class",
                                )
                            })?;
                            Some(crate::storage::IndexOperatorClass::Gist(
                                crate::sql::types::GistOperatorClass::from_code(code).ok_or(
                                    CheckpointSetupError::Corrupt(
                                        "bad gist resolved index operator class",
                                    ),
                                )?,
                            ))
                        } else if let Some(code) = encoded.strip_prefix('n') {
                            let code = code.parse().map_err(|_| {
                                CheckpointSetupError::Corrupt(
                                    "bad gin resolved index operator class",
                                )
                            })?;
                            Some(crate::storage::IndexOperatorClass::Gin(
                                crate::sql::types::GinOperatorClass::from_code(code).ok_or(
                                    CheckpointSetupError::Corrupt(
                                        "bad gin resolved index operator class",
                                    ),
                                )?,
                            ))
                        } else if let Some(code) = encoded.strip_prefix('s') {
                            let code = code.parse().map_err(|_| {
                                CheckpointSetupError::Corrupt(
                                    "bad spgist resolved index operator class",
                                )
                            })?;
                            Some(crate::storage::IndexOperatorClass::SpGist(
                                crate::sql::types::SpGistOperatorClass::from_code(code).ok_or(
                                    CheckpointSetupError::Corrupt(
                                        "bad spgist resolved index operator class",
                                    ),
                                )?,
                            ))
                        } else {
                            return Err(CheckpointSetupError::Corrupt(
                                "bad resolved index operator class encoding",
                            ));
                        };
                    }
                    let tablespace: u16 = parse_field(words.next(), "idx tablespace")?;
                    let fillfactor = match parse_field(words.next(), "idx fillfactor")? {
                        0 => None,
                        value @ 10..=100 => Some(value),
                        _ => return Err(CheckpointSetupError::Corrupt("bad index fillfactor")),
                    };
                    let deduplicate_items = match parse_field(words.next(), "idx deduplicate")? {
                        0 => None,
                        1 => Some(false),
                        2 => Some(true),
                        _ => return Err(CheckpointSetupError::Corrupt("bad index deduplicate")),
                    };
                    let statistics_count: usize =
                        parse_field(words.next(), "idx statistics count")?;
                    if statistics_count != n_cols {
                        return Err(CheckpointSetupError::Corrupt("bad index statistics count"));
                    }
                    let mut statistics = [-1; crate::storage::MAX_INDEX_COLS];
                    for statistic in statistics.iter_mut().take(n_cols) {
                        *statistic = parse_field(words.next(), "idx statistics")?;
                        if !(-1..=10_000).contains(statistic) {
                            return Err(CheckpointSetupError::Corrupt("bad index statistics"));
                        }
                    }
                    let parent: u16 = parse_field(words.next(), "idx parent")?;
                    let kind = match parse_field(words.next(), "idx kind")? {
                        0 => crate::storage::IndexKind::Ordinary,
                        1 => crate::storage::IndexKind::Partitioned { valid: false },
                        2 => crate::storage::IndexKind::Partitioned { valid: true },
                        _ => return Err(CheckpointSetupError::Corrupt("bad index kind")),
                    };
                    let clustered = match parse_field(words.next(), "idx clustered")? {
                        0 => false,
                        1 => true,
                        _ => return Err(CheckpointSetupError::Corrupt("bad index clustered")),
                    };
                    if clustered && !matches!(kind, crate::storage::IndexKind::Ordinary) {
                        return Err(CheckpointSetupError::Corrupt(
                            "partitioned index cannot be clustered",
                        ));
                    }
                    let replica_identity = match parse_field(words.next(), "idx replica identity")?
                    {
                        0 => false,
                        1 => true,
                        _ => {
                            return Err(CheckpointSetupError::Corrupt(
                                "bad index replica identity",
                            ));
                        }
                    };
                    let pages_per_range = match words.next() {
                        None => None,
                        Some(value) => match parse_field(Some(value), "idx pages per range")? {
                            0 => None,
                            value @ 1..=131_072 => Some(value),
                            _ => {
                                return Err(CheckpointSetupError::Corrupt(
                                    "bad index pages per range",
                                ));
                            }
                        },
                    };
                    let autosummarize =
                        if pages_per_range.is_some() || words.clone().next().is_some() {
                            match parse_field(words.next(), "idx autosummarize")? {
                                0 => None,
                                1 => Some(false),
                                2 => Some(true),
                                _ => {
                                    return Err(CheckpointSetupError::Corrupt(
                                        "bad index autosummarize",
                                    ));
                                }
                            }
                        } else {
                            None
                        };
                    let buffering = if words
                        .clone()
                        .next()
                        .is_some_and(|word| word.starts_with("gb"))
                    {
                        match words.next().and_then(|word| word.strip_prefix("gb")) {
                            Some("0") => None,
                            Some("1") => Some(crate::sql::ast::GistBuffering::Auto),
                            Some("2") => Some(crate::sql::ast::GistBuffering::On),
                            Some("3") => Some(crate::sql::ast::GistBuffering::Off),
                            _ => return Err(CheckpointSetupError::Corrupt("bad gist buffering")),
                        }
                    } else {
                        None
                    };
                    let mut operator_class_options =
                        [crate::storage::IndexOperatorClassOptions::DEFAULT;
                            crate::storage::MAX_INDEX_COLS];
                    if words.clone().next() == Some("bo") {
                        let _ = words.next();
                        for options in operator_class_options.iter_mut().take(n_cols) {
                            let encoded = words.next().ok_or(CheckpointSetupError::Corrupt(
                                "idx operator class options missing",
                            ))?;
                            let mut fields = encoded.split(',');
                            let values = fields
                                .next()
                                .and_then(|field| field.strip_prefix('v'))
                                .ok_or(CheckpointSetupError::Corrupt("bad idx values per range"))?;
                            let distinct = fields
                                .next()
                                .and_then(|field| field.strip_prefix('n'))
                                .ok_or(CheckpointSetupError::Corrupt(
                                    "bad idx distinct per range",
                                ))?;
                            let rate = fields
                                .next()
                                .and_then(|field| field.strip_prefix('f'))
                                .ok_or(CheckpointSetupError::Corrupt(
                                    "bad idx false positive rate",
                                ))?;
                            let siglen = match fields.next() {
                                None => None,
                                Some(field) => Some(field.strip_prefix('s').ok_or(
                                    CheckpointSetupError::Corrupt("bad idx gist signature length"),
                                )?),
                            };
                            if fields.next().is_some() {
                                return Err(CheckpointSetupError::Corrupt(
                                    "bad idx operator class options",
                                ));
                            }
                            options.values_per_range = if values == "-" {
                                None
                            } else {
                                let value = values.parse::<u16>().map_err(|_| {
                                    CheckpointSetupError::Corrupt("bad idx values per range")
                                })?;
                                Some(value).filter(|value| (8..=256).contains(value))
                            };
                            if values != "-" && options.values_per_range.is_none() {
                                return Err(CheckpointSetupError::Corrupt(
                                    "bad idx values per range",
                                ));
                            }
                            options.n_distinct_per_range = if distinct == "-" {
                                None
                            } else {
                                let value = distinct.parse::<i32>().map_err(|_| {
                                    CheckpointSetupError::Corrupt("bad idx distinct per range")
                                })?;
                                (value >= -1).then_some(value)
                            };
                            if distinct != "-" && options.n_distinct_per_range.is_none() {
                                return Err(CheckpointSetupError::Corrupt(
                                    "bad idx distinct per range",
                                ));
                            }
                            options.false_positive_rate = if rate == "-" {
                                None
                            } else {
                                let value = rate.parse::<f64>().map_err(|_| {
                                    CheckpointSetupError::Corrupt("bad idx false positive rate")
                                })?;
                                if !value.is_finite() || !(0.000_1..=0.25).contains(&value) {
                                    return Err(CheckpointSetupError::Corrupt(
                                        "bad idx false positive rate",
                                    ));
                                }
                                let stored = StackStr::from_str(rate);
                                if stored.is_truncated() {
                                    return Err(CheckpointSetupError::Corrupt(
                                        "idx false positive rate is too long",
                                    ));
                                }
                                Some(stored)
                            };
                            options.siglen = match siglen {
                                None | Some("-") => None,
                                Some(value) => Some(value.parse::<u16>().map_err(|_| {
                                    CheckpointSetupError::Corrupt("bad idx gist signature length")
                                })?),
                            };
                            if options
                                .siglen
                                .is_some_and(|value| !(1..=2024).contains(&value))
                            {
                                return Err(CheckpointSetupError::Corrupt(
                                    "bad idx gist signature length",
                                ));
                            }
                        }
                    }
                    let (fastupdate, gin_pending_list_limit) = if words
                        .clone()
                        .next()
                        .is_some_and(|word| word.starts_with("gi"))
                    {
                        let encoded = words
                            .next()
                            .and_then(|word| word.strip_prefix("gi"))
                            .ok_or(CheckpointSetupError::Corrupt("bad gin options"))?;
                        let (fast, limit) = encoded
                            .split_once(',')
                            .ok_or(CheckpointSetupError::Corrupt("bad gin options"))?;
                        let fastupdate = match fast {
                            "0" => None,
                            "1" => Some(false),
                            "2" => Some(true),
                            _ => return Err(CheckpointSetupError::Corrupt("bad gin fastupdate")),
                        };
                        let gin_pending_list_limit = match limit {
                            "-" => None,
                            value => {
                                let value = value.parse::<u32>().map_err(|_| {
                                    CheckpointSetupError::Corrupt("bad gin pending list limit")
                                })?;
                                if !(64..=i32::MAX as u32).contains(&value) {
                                    return Err(CheckpointSetupError::Corrupt(
                                        "bad gin pending list limit",
                                    ));
                                }
                                Some(value)
                            }
                        };
                        (fastupdate, gin_pending_list_limit)
                    } else {
                        (None, None)
                    };
                    let mut unsummarized_ranges =
                        [0u64; crate::storage::MAX_BRIN_UNSUMMARIZED_RANGES];
                    let mut unsummarized_count = 0usize;
                    let mut built_pages_per_range = 0u32;
                    let mut summarized_until_page = 0u64;
                    let has_brin_maintenance = words.clone().next() == Some("bu");
                    if has_brin_maintenance {
                        let _ = words.next();
                        built_pages_per_range =
                            parse_field(words.next(), "idx built pages per range")?;
                        if !(1..=131_072).contains(&built_pages_per_range) {
                            return Err(CheckpointSetupError::Corrupt(
                                "bad idx built pages per range",
                            ));
                        }
                        summarized_until_page =
                            parse_field(words.next(), "idx summarized page boundary")?;
                        unsummarized_count =
                            parse_field(words.next(), "idx unsummarized range count")?;
                        if unsummarized_count > unsummarized_ranges.len() {
                            return Err(CheckpointSetupError::Corrupt(
                                "too many idx unsummarized ranges",
                            ));
                        }
                        for range in unsummarized_ranges.iter_mut().take(unsummarized_count) {
                            *range = parse_field(words.next(), "idx unsummarized range")?;
                        }
                    }
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt("trailing idx fields"));
                    }
                    if n_cols < u32::BITS as usize
                        && (descending_mask >> n_cols != 0 || nulls_first_mask >> n_cols != 0)
                    {
                        return Err(CheckpointSetupError::Corrupt("bad index ordering mask"));
                    }
                    let mut descending = [false; crate::storage::MAX_INDEX_COLS];
                    let mut nulls_first = [false; crate::storage::MAX_INDEX_COLS];
                    for i in 0..n_cols {
                        descending[i] = descending_mask & (1u32 << i) != 0;
                        nulls_first[i] = nulls_first_mask & (1u32 << i) != 0;
                    }
                    let method = if resolved_operator_classes[..n_cols].iter().all(|class| {
                        matches!(class, Some(crate::storage::IndexOperatorClass::Hash(_)))
                    }) {
                        crate::sql::ast::IndexAccessMethod::Hash
                    } else if resolved_operator_classes[..n_cols].iter().all(|class| {
                        matches!(class, Some(crate::storage::IndexOperatorClass::Brin(_)))
                    }) {
                        crate::sql::ast::IndexAccessMethod::Brin
                    } else if resolved_operator_classes[..n_cols].iter().all(|class| {
                        matches!(class, Some(crate::storage::IndexOperatorClass::Gist(_)))
                    }) {
                        crate::sql::ast::IndexAccessMethod::Gist
                    } else if resolved_operator_classes[..n_cols].iter().all(|class| {
                        matches!(class, Some(crate::storage::IndexOperatorClass::Gin(_)))
                    }) {
                        crate::sql::ast::IndexAccessMethod::Gin
                    } else if resolved_operator_classes[..n_cols].iter().all(|class| {
                        matches!(class, Some(crate::storage::IndexOperatorClass::SpGist(_)))
                    }) {
                        crate::sql::ast::IndexAccessMethod::SpGist
                    } else if resolved_operator_classes[..n_cols].iter().all(|class| {
                        matches!(
                            class,
                            Some(crate::storage::IndexOperatorClass::Btree(_))
                                | Some(crate::storage::IndexOperatorClass::Catalog(_))
                        )
                    }) {
                        crate::sql::ast::IndexAccessMethod::Btree
                    } else {
                        return Err(CheckpointSetupError::Corrupt(
                            "mixed index access-method operator classes",
                        ));
                    };
                    let slot = storage
                        .create_index(
                            crate::storage::IndexDef {
                                database: crate::storage::DatabaseOid::POSTGRES,
                                created_at,
                                schema: sql_name(&schema)?,
                                name: sql_name(&name)?,
                                pending_name: None,
                                table: sql_name(&table)?,
                                table_binding: None,
                                pending_table: None,
                                method,
                                ownership: crate::storage::Ownership::BOOTSTRAP,
                                columns,
                                expressions,
                                include_columns,
                                collations,
                                explicit_collations,
                                operator_classes,
                                resolved_operator_classes,
                                operator_class_options,
                                descending,
                                nulls_first,
                                n_cols,
                                n_include_cols,
                                nulls_not_distinct,
                                predicate,
                                unique: unique != 0,
                                mutable: crate::storage::IndexMutableDefinition {
                                    tablespace,
                                    options: crate::storage::IndexStorageOptions {
                                        fillfactor,
                                        deduplicate_items,
                                        pages_per_range,
                                        autosummarize,
                                        buffering,
                                        fastupdate,
                                        gin_pending_list_limit,
                                    },
                                    statistics,
                                    parent: (parent != u16::MAX).then_some(parent),
                                    kind,
                                    clustered,
                                    replica_identity,
                                },
                                pending_definition: None,
                                ddl_state: crate::storage::CatalogDdlState::Present,
                            },
                            0,
                        )
                        .map_err(|e| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest index rejected: {}",
                                e.message.as_str()
                            ))
                        })?;
                    // Checkpoint load reconstructs committed state.
                    storage.commit_index_create(slot);
                    if has_brin_maintenance {
                        storage
                            .restore_brin_maintenance(
                                slot,
                                built_pages_per_range,
                                summarized_until_page,
                                &unsummarized_ranges[..unsummarized_count],
                            )
                            .map_err(|error| {
                                CheckpointSetupError::ObjectStore(format!(
                                    "manifest BRIN maintenance state rejected: {}",
                                    error.message.as_str()
                                ))
                            })?;
                    }
                }
                Some("end") => {
                    finish_pending(storage, &mut slot_of, &mut pending_def)?;
                    saw_end = true;
                }
                Some("") | None => {}
                Some(other) => {
                    return Err(CheckpointSetupError::ObjectStore(format!(
                        "unknown manifest line '{other}'"
                    )));
                }
            }
        }
        if !saw_end {
            return Err(CheckpointSetupError::Corrupt("manifest truncated (no end)"));
        }
        storage
            .validate_restored_schema_references()
            .map_err(|error| {
                CheckpointSetupError::ObjectStore(format!(
                    "manifest schema references rejected: {}",
                    error.message.as_str()
                ))
            })?;
        storage
            .validate_restored_catalog_identities(metadata.catalog_sequence)
            .map_err(|error| {
                CheckpointSetupError::ObjectStore(format!(
                    "manifest catalog identities rejected: {}",
                    error.message.as_str()
                ))
            })?;
        storage
            .validate_restored_relation_identities(metadata.catalog_sequence)
            .map_err(|error| {
                CheckpointSetupError::ObjectStore(format!(
                    "manifest relation identities rejected: {}",
                    error.message.as_str()
                ))
            })?;
        storage
            .validate_restored_constraint_identities(metadata.catalog_sequence)
            .map_err(|error| {
                CheckpointSetupError::ObjectStore(format!(
                    "manifest constraint identities rejected: {}",
                    error.message.as_str()
                ))
            })?;
        storage
            .validate_restored_enum_label_identities(metadata.catalog_sequence)
            .map_err(|error| {
                CheckpointSetupError::ObjectStore(format!(
                    "manifest enum identities rejected: {}",
                    error.message.as_str()
                ))
            })?;
        // Block SSTs load in list order. Versioned reads choose the greatest
        // admissible commit LSN across every member.
        bssts.sort_by_key(|(mindex, idx, ..)| (*mindex, *idx));
        for (mindex, idx, count, crc, handle) in &bssts {
            let slot =
                slot_of
                    .get(*mindex)
                    .copied()
                    .flatten()
                    .ok_or(CheckpointSetupError::Corrupt(
                        "dsst references unknown table",
                    ))?;
            if slot >= self.prev_ssts.len() {
                return Err(CheckpointSetupError::Corrupt(
                    "dsst table slot exceeds configured table capacity",
                ));
            }
            let expect = self.prev_ssts[slot].n;
            if let Some(handle) = handle {
                if *idx != expect {
                    return Err(CheckpointSetupError::Corrupt(
                        "dsst list index out of order",
                    ));
                }
                self.rehydrate_block_sst(storage, slot, *idx as u32, *count, handle)?;
                if !self.prev_ssts[slot].push(PrevSst {
                    handle: *handle,
                    count: *count,
                    crc: *crc,
                }) {
                    return Err(CheckpointSetupError::Corrupt(
                        "dsst list exceeds max_spill_generations_per_table",
                    ));
                }
            }
        }
        for (slot, list) in self.prev_ssts.iter().enumerate() {
            if list.n > 0 {
                storage
                    .set_spill_list_from_iter(slot, list.n, list.iter().map(|p| p.handle))
                    .map_err(|_| {
                        CheckpointSetupError::Corrupt(
                            "manifest SST list has invalid length or exceeds table capacity",
                        )
                    })?;
            }
        }
        // Named indexes are serialized after tables, so establish the final
        // tuple-binding shape before attaching physical generations.
        storage.rebuild_recovered_enforcers().map_err(|error| {
            CheckpointSetupError::ObjectStore(format!(
                "manifest value-index bindings rejected: {}",
                error.message.as_str()
            ))
        })?;
        for (mindex, columns, n_columns, index_created_at, include_mask, handle) in value_indexes {
            let slot =
                slot_of
                    .get(mindex)
                    .copied()
                    .flatten()
                    .ok_or(CheckpointSetupError::Corrupt(
                        "vix references unknown table",
                    ))?;
            storage
                .install_value_binding(
                    slot,
                    &columns[..n_columns],
                    index_created_at,
                    include_mask,
                    Some(handle),
                    crate::storage::ValueIndexInstallation::Recovery,
                )
                .map_err(|error| {
                    CheckpointSetupError::ObjectStore(format!(
                        "manifest value index rejected: {}",
                        error.message.as_str()
                    ))
                })?;
        }
        if relation_index_hints.len() != slot_of.iter().flatten().count() {
            return Err(CheckpointSetupError::Corrupt(
                "manifest relation index hints are incomplete",
            ));
        }
        for (manifest_index, has_index) in relation_index_hints {
            let slot = slot_of.get(manifest_index).copied().flatten().ok_or(
                CheckpointSetupError::Corrupt("relation index hint references unknown table"),
            )?;
            let database = storage.table(slot).database();
            if slot == storage.large_object_page_table() {
                if has_index {
                    return Err(CheckpointSetupError::Corrupt(
                        "internal large-object relation cannot have a user index hint",
                    ));
                }
                continue;
            }
            storage
                .select_database_for_recovery(database)
                .map_err(|_| {
                    CheckpointSetupError::Corrupt("unknown relation index hint database")
                })?;
            storage
                .replay_relation_index_hint(storage.table_position(slot), has_index, 0)
                .map_err(|_| CheckpointSetupError::Corrupt("invalid relation index hint source"))?;
        }
        for (manifest_index, statistics) in table_statistics {
            let slot = slot_of.get(manifest_index).copied().flatten().ok_or(
                CheckpointSetupError::Corrupt("statistics reference unknown table"),
            )?;
            if statistics.rows > usize::MAX as u64 {
                return Err(CheckpointSetupError::Corrupt(
                    "statistics row count exceeds addressable memory",
                ));
            }
            let n_columns = storage.table(slot).definition().n_columns;
            if statistics.columns[n_columns..]
                .iter()
                .any(|column| column.valid)
            {
                return Err(CheckpointSetupError::Corrupt(
                    "statistics reference a nonexistent column",
                ));
            }
            storage.install_table_statistics(slot, statistics);
        }
        for statistics in extended_statistics {
            storage
                .select_database_for_recovery(statistics.database)
                .map_err(|_| CheckpointSetupError::Corrupt("unknown statistics database"))?;
            let table = slot_of
                .get(statistics.table_index)
                .copied()
                .flatten()
                .ok_or(CheckpointSetupError::Corrupt(
                    "extended statistics reference unknown table",
                ))?;
            if statistics.keys[..usize::from(statistics.n_keys)]
                .iter()
                .any(|key| {
                    matches!(key, crate::storage::ExtendedStatisticsKey::Column(column)
                    if storage.table(table).definition().column_index(column.as_str()).is_none())
                })
            {
                return Err(CheckpointSetupError::Corrupt(
                    "extended statistics reference a nonexistent column",
                ));
            }
            let slot = storage
                .replay_extended_statistics(crate::storage::ExtendedStatisticsSpec {
                    creation: crate::storage::CatalogCreation::Restore(
                        crate::storage::CatalogIdentity::require(statistics.created_at).map_err(
                            |_| {
                                CheckpointSetupError::Corrupt(
                                    "invalid statistics creation identity",
                                )
                            },
                        )?,
                    ),
                    schema: statistics.schema,
                    name: statistics.name,
                    table: table as u16,
                    target: statistics.target,
                    keys: statistics.keys,
                    n_keys: statistics.n_keys,
                    kinds: statistics.kinds,
                    expression_only: statistics.expression_only,
                })
                .map_err(|error| {
                    CheckpointSetupError::ObjectStore(format!(
                        "manifest extended statistics rejected: {}",
                        error.message.as_str()
                    ))
                })?;
            if statistics.data.valid {
                storage.install_extended_statistics_data(slot, statistics.data);
            }
        }
        // Engine startup performs one catalog rebind after it has merged the
        // manifest and every committed journal source. Rebinding here would
        // validate an intermediate image and retain this parser's large frame
        // during the same database-wide work.
        storage
            .select_database(crate::storage::DatabaseOid::POSTGRES)
            .map_err(|_| CheckpointSetupError::Corrupt("postgres database is unavailable"))?;
        storage.set_lsn(lsn);
        if next_rowid > 0 {
            storage.observe_rowid(next_rowid - 1);
        }
        storage.observe_transaction_identity(metadata.latest_transaction_id);
        self.manifest_lsn = lsn;
        Ok(lsn)
    }

    /// Verifies one block-grid SST's roots and advances the global rowid floor.
    /// Cold start installs only the ordered handle list: rows and historical
    /// versions remain object-resident, so neither the heap nor the row-map
    /// capacity bounds recovery.
    fn rehydrate_block_sst(
        &mut self,
        storage: &mut Storage,
        slot: usize,
        sst_index: u32,
        count: u64,
        handle: &SstHandle,
    ) -> Result<(), CheckpointSetupError> {
        let _ = slot;
        // The row map is an overlay, not an index: SST-resident rows need no
        // entries, so loading an SST installs nothing — the spill list alone
        // makes its rows reachable, and cold start costs O(manifest), not
        // O(rows). What must still happen here: the SST's root blocks are
        // verified reachable (fail at startup, not mid-query), and the
        // rowid floor advances past everything the SST holds so no new row
        // can collide with a stored one. The last data block's final key is
        // the SST's maximum, found through the sparse index — three block
        // reads however large the table. (Per-block checksums verify every
        // later read; the old whole-SST scan's CRC pass went with it.)
        let _ = (count, sst_index);
        self.sst_arena.reset();
        let index_buf = self
            .sst_arena
            .alloc_slice_with(crate::store::MAX_PAYLOAD, |_| 0u8)
            .map_err(|_| CheckpointSetupError::Corrupt("sst reader scratch"))?;
        let data_buf = self
            .sst_arena
            .alloc_slice_with(crate::store::MAX_PAYLOAD, |_| 0u8)
            .map_err(|_| CheckpointSetupError::Corrupt("sst reader scratch"))?;
        let mut blocks = self.blocks.lock().expect("block store lock poisoned");
        let block_count = crate::store::data_block_total(&mut *blocks, handle, index_buf)
            .map_err(|_| CheckpointSetupError::Corrupt("sst index unreachable"))?;
        if block_count == 0 {
            return Err(CheckpointSetupError::Corrupt("sst index names no blocks"));
        }
        let last_reference =
            crate::store::locate_data_block_ref(&mut *blocks, handle, index_buf, block_count - 1)
                .map_err(|_| CheckpointSetupError::Corrupt("sst index unreachable"))?
                .ok_or(CheckpointSetupError::Corrupt("sst index names no blocks"))?;
        let mut max_rowid: Option<u64> = None;
        let (data_len, block_type) = crate::store::read_data_block_raw_ref(
            &mut *blocks,
            last_reference,
            data_buf,
            index_buf,
        )
        .map_err(|_| CheckpointSetupError::Corrupt("sst data block unreachable"))?;
        if block_type == crate::store::BlockType::SstDataPaxV2 {
            let layout = crate::store::pax_layout(&data_buf[..data_len])
                .map_err(|_| CheckpointSetupError::Corrupt("sst PAX descriptor unreachable"))?;
            for row in 0..layout.rows() {
                let (key, _) = layout
                    .row_key(&data_buf[..data_len], row)
                    .map_err(|_| CheckpointSetupError::Corrupt("sst PAX descriptor unreachable"))?;
                max_rowid = Some(key.rowid);
            }
        } else {
            let decoded =
                crate::store::decode_data_block(&data_buf[..data_len], block_type, index_buf)
                    .map_err(|_| CheckpointSetupError::Corrupt("sst data block unreachable"))?;
            let mut at = 0usize;
            while let Some((key, _, _, next)) =
                crate::store::block_keys_at(&index_buf[..decoded], at)
            {
                max_rowid = Some(key.rowid);
                at = next;
            }
        }
        drop(blocks);
        if let Some(rowid) = max_rowid {
            storage.observe_rowid(rowid);
        }
        Ok(())
    }

    /// Uploads and publishes a full snapshot. The publication result is
    /// returned immediately so the caller can make the matching local WAL,
    /// heap, and overlay transition before another statement runs. Remote
    /// garbage maintenance remains pending and is either paced by later beats
    /// or drained explicitly by [`Self::finish_maintenance`].
    pub(crate) fn checkpoint(&mut self, storage: &mut Storage) -> Result<Option<u64>, SqlError> {
        if self.credential_install_pending() {
            return Err(sql_err!(
                sqlstate::INTERNAL_IO_WAIT,
                "checkpoint waits for validated credential installation"
            ));
        }
        loop {
            match self.checkpoint_step(storage)? {
                CheckpointStep::Idle => return Ok(None),
                CheckpointStep::Working => {
                    if self.manifest_publication_pending() {
                        self.wait_manifest_publication()?;
                    } else if !self.block_output.is_empty() {
                        self.block_output
                            .wait_ready(&mut self.block_uploads)
                            .map_err(upload_to_sql)?;
                    }
                }
                CheckpointStep::Published { lsn } => return Ok(Some(lsn)),
            }
        }
    }

    /// Whether a sweep is mid-flight — once true, every beat advances it
    /// until the manifest publishes, trigger conditions or not.
    pub(crate) fn sweep_active(&self) -> bool {
        self.sweeping || self.requested_checkpoint_epoch > self.published_checkpoint_epoch
    }

    fn observe_retained_root_release(&mut self, storage: &Storage) {
        if !self.block_garbage_pending && storage.take_retained_file_reclamation() {
            self.block_garbage_pending = true;
            self.published_lsn_pending_maintenance
                .get_or_insert(storage.lsn());
        }
    }

    /// Drains the bounded post-publication deletion batches synchronously.
    /// Explicit `CHECKPOINT` and shutdown use this after applying their local
    /// publication transition; ordinary event-loop beats use the paced path.
    pub(crate) fn finish_maintenance(&mut self, storage: &Storage) -> Result<(), SqlError> {
        if self.credential_install_pending() {
            return Err(sql_err!(
                sqlstate::INTERNAL_IO_WAIT,
                "maintenance waits for validated credential installation"
            ));
        }
        self.observe_retained_root_release(storage);
        while self.published_lsn_pending_maintenance.is_some() || self.commit_cleanup_pending() {
            self.metadata_io.wait_ready()?;
            self.advance_commit_cleanup()?;
            if self.published_lsn_pending_maintenance.is_none() {
                continue;
            }
            if self.legacy_garbage_pending {
                self.legacy_garbage_pending = !self.collect_garbage_batch()?;
                continue;
            }
            if self.block_garbage_pending {
                self.block_garbage_pending = !self.collect_block_garbage_batch(storage)?;
                continue;
            }
            self.published_lsn_pending_maintenance = None;
        }
        Ok(())
    }

    /// Advances captured table/index output, manifest publication or cleanup.
    /// Changed tables need a current slice before manifest capture. Upload and
    /// local installation then retain that capture's identity across turns.
    /// Some capture and block-cleanup reads still wait synchronously.
    ///
    /// Failures retain a retryable position or restart the same immutable job.
    /// Repeated PUTs keep the same block identity but still cost requests and
    /// bandwidth. Unpublished blocks remain orphans for a later garbage sweep.
    pub(crate) fn checkpoint_step(
        &mut self,
        storage: &mut Storage,
    ) -> Result<CheckpointStep, SqlError> {
        if self.manifest_publication_pending() {
            return self.advance_manifest_publication(storage);
        }
        if !self.advance_physical_output(storage)? {
            return Ok(CheckpointStep::Working);
        }
        if self.table_job.is_some() {
            self.advance_retained_output(storage)?;
            return Ok(CheckpointStep::Working);
        }
        self.observe_retained_root_release(storage);
        if self
            .merge_job
            .as_ref()
            .is_some_and(|job| matches!(job.phase, MergePhase::Finish { .. }))
        {
            self.merge_beat(storage)?;
            return Ok(CheckpointStep::Working);
        }
        if self.published_lsn_pending_maintenance.is_some() {
            if self.legacy_garbage_pending {
                self.legacy_garbage_pending = !self.collect_garbage_batch()?;
                return Ok(CheckpointStep::Working);
            }
            if self.block_garbage_pending {
                self.block_garbage_pending = !self.collect_block_garbage_batch(storage)?;
                return Ok(CheckpointStep::Working);
            }
            self.published_lsn_pending_maintenance = None;
            return Ok(CheckpointStep::Working);
        }
        self.reconcile_published_spill_lists(storage);
        let dirty_full_list = (0..storage.physical_table_count()).any(|slot| {
            storage.table(slot).live()
                && storage.table(slot).definition().persistence
                    != crate::storage::RelationPersistence::Temporary
                && storage.table(slot).dirty()
                && self.merge_done[slot].is_none()
                && self
                    .prev_ssts
                    .get(slot)
                    .is_some_and(|list| list.n == list.capacity())
        });
        if dirty_full_list {
            if self.merge_job.is_none() && self.merge_candidate(storage).is_none() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "dirty full SST generation list requires a merge exceeding checkpoint_merge_entries ({})",
                    self.merge_scratch.capacity()
                ));
            }
            self.merge_beat(storage)?;
            return Ok(CheckpointStep::Working);
        }
        // Merge beats interleave with sweep work — alternating when both
        // want the engine, so a hot sweep cannot starve compaction and a
        // long merge cannot starve publishes. A finished merge makes a
        // sweep due even at an unchanged lsn: its install needs a publish.
        let merge_due = self.merge_job.is_some() || self.merge_candidate(storage).is_some();
        let value_index_due = self.value_index_schedule_pending(storage);
        let sweep_due = self.sweep_active()
            || storage.lsn() != self.manifest_lsn
            || !self.manifest_present
            || self.merge_done.iter().any(Option::is_some)
            || value_index_due
            || storage.statistics_dirty();
        if merge_due && (self.merge_turn || !sweep_due) {
            self.merge_turn = false;
            self.merge_beat(storage)?;
            return Ok(CheckpointStep::Working);
        }
        self.merge_turn = true;
        if !sweep_due {
            return Ok(CheckpointStep::Idle);
        }
        if !self.sweeping {
            self.sweeping = true;
            self.sliced_generation.iter_mut().for_each(|g| *g = 0);
            self.sliced_lsn.iter_mut().for_each(|lsn| *lsn = 0);
            self.sliced_layout
                .iter_mut()
                .for_each(|layout| *layout = SliceLayout::EMPTY);
            self.sliced_this_sweep.iter_mut().for_each(|s| *s = false);
            self.checkpoint_deletions
                .iter_mut()
                .for_each(|guard| *guard = None);
            self.pending_installs.clear();
            self.pending_value_installs.clear();
            self.value_schedule_job = None;
            self.value_job = None;
            self.value_changed_rowids.clear();
            self.value_writer.reset();
        }
        if self.value_schedule_job.is_some() {
            self.value_index_schedule_beat(storage)?;
            return Ok(CheckpointStep::Working);
        }
        if self.value_job.is_some() {
            self.value_index_write_beat(storage)?;
            return Ok(CheckpointStep::Working);
        }
        let mut wrote_slice = false;
        for slot in 0..storage.physical_table_count() {
            let table = storage.table(slot);
            let skip_table = {
                let definition = table.definition();
                !definition.identity().live()
                    || definition.persistence == crate::storage::RelationPersistence::Temporary
            };
            if skip_table {
                continue;
            }
            if self.start_value_index_job(storage, slot)? {
                return Ok(CheckpointStep::Working);
            }
            if !self.needs_slice(storage, slot) {
                continue;
            }
            if !self.begin_table_slice(storage, slot)? {
                return Ok(CheckpointStep::Working);
            }
            wrote_slice = true;
            break;
        }
        if wrote_slice
            && (merge_due
                || (0..storage.physical_table_count()).any(|slot| self.needs_slice(storage, slot))
                || self.value_index_schedule_pending(storage))
        {
            return Ok(CheckpointStep::Working);
        }
        // Without a merge beat due, publish with the final slice while
        // every captured generation is still current. Yielding here lets the
        // next statement invalidate that slice and forces its immutable
        // blocks to be written again. An owed merge retains its alternating
        // beat so long histories keep making bounded progress.
        let lsn = storage.lsn();
        #[cfg(feature = "checkpoint-profile")]
        let publish_started = checkpoint_profile_start();
        #[cfg(feature = "checkpoint-profile")]
        let publish_before = self
            .blocks
            .lock()
            .expect("block store lock poisoned")
            .io_stats();
        let published = self.prepare_manifest(storage, lsn);
        #[cfg(feature = "checkpoint-profile")]
        profile_checkpoint_phase(
            "manifest",
            lsn,
            None,
            publish_started,
            publish_before,
            self.blocks
                .lock()
                .expect("block store lock poisoned")
                .io_stats(),
            0,
        );
        published?;
        self.queue_manifest_upload(lsn)?;
        Ok(CheckpointStep::Working)
    }

    /// For a live relation, storage's installed handles and `prev_ssts` name
    /// the same published generation. A DROP, unlogged crash reset, or slot
    /// reuse deliberately clears storage first; carrying the old per-slot
    /// list forward would then make the new relation a delta of its retired
    /// occupant and resurrect those rows on the next cold start.
    ///
    /// Reconcile before merge selection as well as slicing: a stale list at
    /// the compaction trigger must not start reading the old relation under
    /// the replacement's schema. Any slice or merge already built for the
    /// retired identity is now an orphan and is discarded before publish.
    fn reconcile_published_spill_lists(&mut self, storage: &Storage) {
        for slot in 0..storage.physical_table_count() {
            let Some(published) = self.prev_ssts.get(slot) else {
                continue;
            };
            if published.n == 0 {
                continue;
            }
            let table = storage.table(slot);
            let generations = table.spill_generations();
            let matches_storage = table.live()
                && {
                    let definition = table.definition();
                    definition.persistence
                } != crate::storage::RelationPersistence::Temporary
                && generations.len() == published.n
                && (0..published.n).all(|index| {
                    generations.handle(index) == published.ssts[index].map(|prior| prior.handle)
                });
            if matches_storage {
                continue;
            }

            self.prev_ssts[slot].clear();
            if slot < self.prev_scratch.len() {
                self.prev_scratch[slot].clear();
            }
            self.pending_installs
                .retain(|(pending_slot, _)| *pending_slot != slot);
            self.pending_value_installs
                .retain(|install| install.slot != slot);
            self.sliced_generation[slot] = 0;
            self.sliced_lsn[slot] = 0;
            self.sliced_layout[slot] = SliceLayout::EMPTY;
            self.sliced_this_sweep[slot] = false;
            if self.merge_job.as_ref().is_some_and(|job| job.slot == slot) {
                self.merge_job = None;
            }
            self.merge_done[slot] = None;
            self.merge_overflow[slot] = None;
        }
    }

    /// Whether `slot` still needs a slice this sweep: it changed since its
    /// slice, or was never sliced while dirty. (Compaction is the merge
    /// job's business, not the sweep's.)
    fn needs_slice(&self, storage: &Storage, slot: usize) -> bool {
        let table = storage.table(slot);
        table.live()
            && {
                let definition = table.definition();
                definition.persistence
            } != crate::storage::RelationPersistence::Temporary
            && table.dirty()
            && self.sliced_generation[slot] != table.generation()
    }

    /// Assembles and publishes the manifest from the sweep's recorded
    /// per-table lists. The retained bytes stay unchanged until publication
    /// resolves. Runs only when no table has an outdated slice.
    fn prepare_manifest(&mut self, storage: &Storage, lsn: u64) -> Result<(), SqlError> {
        if self.writer_was_fenced() {
            return Err(sql_err!(
                SQLSTATE_CAS,
                "writer was fenced by a newer process"
            ));
        }
        // Delta bookkeeping collects the new per-slot references into
        // pre-reserved scratch so this post-freeze path never allocates.
        for (slot, receipt) in self.checkpoint_statistics.iter_mut().enumerate() {
            *receipt = Some(storage.capture_checkpoint_statistics(slot));
            self.sliced_layout[slot] = SliceLayout::from_storage(storage, slot);
        }
        self.manifest_output.clear();
        write_manifest(&mut self.manifest_output, MANIFEST_HEADER)?;
        write_manifest(&mut self.manifest_output, format_args!("lsn {lsn}"))?;
        write_manifest(
            &mut self.manifest_output,
            format_args!("catalog_sequence {}", storage.catalog_identity_sequence()),
        )?;
        write_manifest(
            &mut self.manifest_output,
            format_args!("next_rowid {}", storage.peek_next_rowid()),
        )?;
        write_manifest(
            &mut self.manifest_output,
            format_args!("role_sequence {}", storage.role_identity_sequence()),
        )?;
        let mut identity_result = Ok(());
        storage.visit_transaction_identity_state(|record| {
            if identity_result.is_err() {
                return;
            }
            identity_result = match record {
                TransactionIdentityRecord::Latest(transaction_id) => write_manifest(
                    &mut self.manifest_output,
                    format_args!("latest_transaction_id {transaction_id}"),
                ),
                TransactionIdentityRecord::Recent {
                    transaction_id,
                    committed,
                } => write_manifest(
                    &mut self.manifest_output,
                    format_args!(
                        "transaction_status {} {}",
                        transaction_id,
                        if committed { "committed" } else { "aborted" }
                    ),
                ),
            };
        });
        identity_result?;
        match storage.next_large_object_oid() {
            Some(oid) => write_manifest(
                &mut self.manifest_output,
                format_args!("next_lo_oid {}", oid.get()),
            )?,
            None => write_manifest(&mut self.manifest_output, "next_lo_oid -")?,
        }
        write_manifest(
            &mut self.manifest_output,
            format_args!("writer {:032x}", self.writer_token.0),
        )?;
        let mut database_context = None;

        // Roles are durable catalog authority. Parsed verifier material crosses
        // this object-backed manifest; plaintext passwords never do.
        for (slot, role) in storage.live_roles() {
            use core::fmt::Write;
            let attributes = role.attributes;
            let mut name = StackStr::<130>::new();
            for byte in role.name.as_str().as_bytes() {
                let _ = write!(name, "{byte:02x}");
            }
            let mut credential_kind = StackStr::<1>::new();
            let mut salt = StackStr::<{ 2 * crate::pg::auth::SCRAM_SALT_MAX }>::new();
            let mut stored_key = StackStr::<64>::new();
            let mut server_key = StackStr::<64>::new();
            let mut iterations = 0u32;
            match attributes.password {
                None => {
                    let _ = write!(credential_kind, "-");
                    let _ = write!(salt, "-");
                    let _ = write!(stored_key, "-");
                    let _ = write!(server_key, "-");
                }
                Some(crate::storage::RoleCredential::Scram(password)) => {
                    let _ = write!(credential_kind, "s");
                    for byte in password.salt.as_bytes() {
                        let _ = write!(salt, "{byte:02x}");
                    }
                    for byte in password.stored_key {
                        let _ = write!(stored_key, "{byte:02x}");
                    }
                    for byte in password.server_key {
                        let _ = write!(server_key, "{byte:02x}");
                    }
                    iterations = password.iterations;
                }
                Some(crate::storage::RoleCredential::Md5(password)) => {
                    let _ = write!(credential_kind, "m");
                    let _ = salt.write_str(
                        core::str::from_utf8(&password.hash).expect("MD5 verifier is ASCII"),
                    );
                    let _ = write!(stored_key, "-");
                    let _ = write!(server_key, "-");
                }
            }
            let mut valid_until = StackStr::<{ 2 * crate::storage::ROLE_VALID_UNTIL_MAX }>::new();
            if let Some(value) = attributes.valid_until.as_ref() {
                if value.as_str().is_empty() {
                    let _ = write!(valid_until, "0");
                } else {
                    for byte in value.as_str().as_bytes() {
                        let _ = write!(valid_until, "{byte:02x}");
                    }
                }
            } else {
                let _ = write!(valid_until, "-");
            }
            let flags = u16::from(attributes.superuser)
                | (u16::from(attributes.inherit) << 1)
                | (u16::from(attributes.create_role) << 2)
                | (u16::from(attributes.create_database) << 3)
                | (u16::from(attributes.can_login) << 4)
                | (u16::from(attributes.replication) << 5)
                | (u16::from(attributes.bypass_row_level_security) << 6)
                | (u16::from(attributes.password.is_some()) << 7)
                | (u16::from(attributes.valid_until.is_some()) << 8);
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "rol {} {} {} {} {} {} {} {} {} {} {}",
                    role.oid().get(),
                    slot,
                    name.as_str(),
                    flags,
                    attributes.connection_limit,
                    credential_kind.as_str(),
                    salt.as_str(),
                    stored_key.as_str(),
                    server_key.as_str(),
                    iterations,
                    valid_until.as_str()
                ),
            )?;
        }
        for (slot, membership) in storage.live_role_memberships() {
            let flags = u8::from(membership.options.admin)
                | (u8::from(membership.options.inherit) << 1)
                | (u8::from(membership.options.set) << 2);
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "rmem {} {} {} {} {} {}",
                    slot,
                    membership.created_at,
                    storage.role(usize::from(membership.role)).oid().get(),
                    storage.role(usize::from(membership.member)).oid().get(),
                    storage.role(usize::from(membership.grantor)).oid().get(),
                    flags
                ),
            )?;
        }
        for (_, database) in storage.databases_visible_to(0) {
            use core::fmt::Write;
            let definition = database.definition_for(0);
            let owner = usize::from(database.ownership.owner_to(0));
            let hex = |value: &str| {
                let mut encoded = StackStr::<256>::new();
                if value.is_empty() {
                    let _ = write!(encoded, "-");
                } else {
                    for byte in value.as_bytes() {
                        let _ = write!(encoded, "{byte:02x}");
                    }
                }
                encoded
            };
            let name = hex(definition.name.as_str());
            let owner = hex(storage.role_name(owner, 0).as_str());
            let collate = hex(definition.collate.as_str());
            let ctype = hex(definition.ctype.as_str());
            let locale = hex(definition.locale.as_str());
            let collation_version = hex(definition.collation_version.as_str());
            let flags =
                u8::from(definition.allow_connections) | (u8::from(definition.is_template) << 1);
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "db {} {} {} {} {} {} {} {} {} {} {} {}",
                    database.oid.get(),
                    name.as_str(),
                    owner.as_str(),
                    flags,
                    definition.encoding.code(),
                    definition.locale_provider.code(),
                    definition.tablespace,
                    definition.connection_limit,
                    collate.as_str(),
                    ctype.as_str(),
                    locale.as_str(),
                    collation_version.as_str()
                ),
            )?;
        }
        for (_, setting) in storage.role_settings() {
            if !setting.live {
                continue;
            }
            use core::fmt::Write;
            let (scope, role_slot, database) = match setting.scope {
                crate::storage::RoleSettingScope::RoleAllDatabases(role) => (0, Some(role), None),
                crate::storage::RoleSettingScope::RoleInDatabase { role, database } => {
                    (1, Some(role), Some(database))
                }
                crate::storage::RoleSettingScope::AllRolesInDatabase(database) => {
                    (2, None, Some(database))
                }
            };
            let mut role = StackStr::<130>::new();
            if let Some(slot) = role_slot {
                for byte in storage.role(slot as usize).name.as_str().as_bytes() {
                    let _ = write!(role, "{byte:02x}");
                }
            } else {
                let _ = write!(role, "-");
            }
            let mut name = StackStr::<130>::new();
            for byte in setting.name.as_str().as_bytes() {
                let _ = write!(name, "{byte:02x}");
            }
            let mut value = StackStr::<{ crate::storage::ROLE_SETTING_VALUE_MAX * 2 }>::new();
            let mut database_text = StackStr::<16>::new();
            if let Some(database) = database {
                let _ = write!(database_text, "{}", database.get());
            } else {
                let _ = write!(database_text, "-");
            }
            if setting.value.as_str().is_empty() {
                let _ = write!(value, "0");
            } else {
                for byte in setting.value.as_str().as_bytes() {
                    let _ = write!(value, "{byte:02x}");
                }
            }
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "rset {} {} {} {} {}",
                    scope,
                    role.as_str(),
                    database_text.as_str(),
                    name.as_str(),
                    value.as_str()
                ),
            )?;
        }
        for (_, setting) in storage.system_settings() {
            if !setting.live {
                continue;
            }
            use core::fmt::Write;
            let mut name = StackStr::<130>::new();
            for byte in setting.name.as_str().as_bytes() {
                let _ = write!(name, "{byte:02x}");
            }
            let mut value = StackStr::<{ crate::storage::ROLE_SETTING_VALUE_MAX * 2 }>::new();
            if setting.value.as_str().is_empty() {
                let _ = write!(value, "-");
            } else {
                for byte in setting.value.as_str().as_bytes() {
                    let _ = write!(value, "{byte:02x}");
                }
            }
            write_manifest(
                &mut self.manifest_output,
                format_args!("sset {} {}", name.as_str(), value.as_str()),
            )?;
        }
        storage.copy_prepared_transaction_catalog(&mut self.prepared_transaction_snapshot)?;
        for prepared in self.prepared_transaction_snapshot.iter() {
            use core::fmt::Write;
            let mut gid = StackStr::<398>::new();
            if prepared.gid.as_str().is_empty() {
                let _ = write!(gid, "-");
            } else {
                for byte in prepared.gid.as_str().as_bytes() {
                    let _ = write!(gid, "{byte:02x}");
                }
            }
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "ptx {} {} {} {} {} {} {}",
                    prepared.transaction_id,
                    prepared.first_lsn,
                    prepared.prepared_lsn,
                    prepared.prepared_at,
                    prepared.database.get(),
                    prepared.owner.get(),
                    gid.as_str()
                ),
            )?;
        }

        // Even initial namespaces are explicit: public can be renamed or dropped.
        for (_, schema) in storage.checkpoint_schemas() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                schema.database,
            )?;
            use core::fmt::Write;
            let mut hex = StackStr::<130>::new();
            for b in schema.name.as_str().as_bytes() {
                let _ = write!(hex, "{b:02x}");
            }
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "nsp {} {} {}",
                    schema
                        .identity()
                        .expect("checkpoint selects a schema")
                        .created_at(),
                    u8::from(schema.has_database_owner(0)),
                    hex.as_str()
                ),
            )?;
        }
        for (_, object) in storage.checkpoint_large_objects() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                object.database,
            )?;
            write_manifest(
                &mut self.manifest_output,
                format_args!("lob {} {}", object.oid.get(), object.created_at),
            )?;
        }
        // Domains: `dom4 <base-code> <base-typmod> <not-null> <n-checks>
        // <hex-schema> <hex-name> <hex-base-domain> <hex-base-domain-schema>
        // <hex-base-type> <hex-base-type-schema> <hex-default>
        // [<hex-cname> <hex-cexpr> <validation>]...`. Like enums, domains precede tables
        // because generated domain-array columns bind their runtime slot while
        // the table definition is rebuilt.
        for (slot, d) in storage.checkpoint_domains() {
            crate::storage::validate_constraint_identities(d.constraint_identities())?;
            let parent_name = d
                .base_domain
                .map(crate::storage::UserTypeName::storage_name);
            let base_name = d
                .base_user_type
                .map(crate::storage::UserTypeName::storage_name);
            write_database_context(&mut self.manifest_output, &mut database_context, d.database)?;
            use core::fmt::Write;
            write!(
                &mut self.manifest_output,
                "dom6 {} {} {} {} {} {}",
                slot,
                d.created_at,
                d.base.code(),
                d.base_type_mod,
                u8::from(d.not_null),
                d.n_checks
            )
            .map_err(|_| manifest_full())?;
            for value in [
                d.schema.as_str(),
                d.name.as_str(),
                parent_name.as_ref().map_or("", |name| name.as_str()),
                d.base_domain
                    .as_ref()
                    .map_or("", |identity| identity.schema.as_str()),
                base_name.as_ref().map_or("", |name| name.as_str()),
                d.base_user_type
                    .as_ref()
                    .map_or("", |identity| identity.schema.as_str()),
                d.not_null_name.as_ref().map_or("", |name| name.as_str()),
                d.default_expr
                    .as_ref()
                    .map_or("", |expression| expression.as_str()),
            ] {
                write!(&mut self.manifest_output, " ").map_err(|_| manifest_full())?;
                write!(&mut self.manifest_output, "{}", ManifestZeroName(value))
                    .map_err(|_| manifest_full())?;
            }
            for check in d.checks() {
                for value in [check.name.as_str(), check.expression.as_str()] {
                    write!(&mut self.manifest_output, " ").map_err(|_| manifest_full())?;
                    write!(&mut self.manifest_output, "{}", ManifestZeroName(value))
                        .map_err(|_| manifest_full())?;
                }
                write!(&mut self.manifest_output, " {}", check.validation.code())
                    .map_err(|_| manifest_full())?;
            }
            for identity in d.constraint_identities() {
                let identity = identity.ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "checkpoint constraint has no creation identity"
                    )
                })?;
                write!(&mut self.manifest_output, " {}", identity.created_at())
                    .map_err(|_| manifest_full())?;
            }
            writeln!(&mut self.manifest_output).map_err(|_| manifest_full())?;
        }
        // Enum types: `enm <hex-schema> <hex-name> <n-members> [<hex-label>
        // <sort-bits>]...`. Written before tables so an enum-typed column
        // resolves its type slot when its table is rebuilt on load. The sort
        // key is emitted as its exact f64 bit pattern.
        for (slot, e) in storage.checkpoint_enums() {
            let definition = storage.enum_for(slot, 0);
            write_database_context(&mut self.manifest_output, &mut database_context, e.database)?;
            use core::fmt::Write;
            write!(&mut self.manifest_output, "enm3 {} {} ", slot, e.created_at)
                .map_err(|_| manifest_full())?;
            write!(
                &mut self.manifest_output,
                "{}",
                ManifestZeroName(e.schema.as_str())
            )
            .map_err(|_| manifest_full())?;
            write!(&mut self.manifest_output, " ").map_err(|_| manifest_full())?;
            write!(
                &mut self.manifest_output,
                "{}",
                ManifestZeroName(e.name.as_str())
            )
            .map_err(|_| manifest_full())?;
            write!(&mut self.manifest_output, " {}", definition.members().len())
                .map_err(|_| manifest_full())?;
            for m in definition.members() {
                write!(&mut self.manifest_output, " ").map_err(|_| manifest_full())?;
                write!(
                    &mut self.manifest_output,
                    "{}",
                    ManifestZeroName(m.label.as_str())
                )
                .map_err(|_| manifest_full())?;
                write!(&mut self.manifest_output, " {}", m.sort.to_bits())
                    .map_err(|_| manifest_full())?;
                write!(&mut self.manifest_output, " ").map_err(|_| manifest_full())?;
                match m.legacy_label {
                    Some(label) => write!(
                        &mut self.manifest_output,
                        "{}",
                        ManifestZeroName(label.as_str())
                    )
                    .map_err(|_| manifest_full())?,
                    None => write!(&mut self.manifest_output, "-").map_err(|_| manifest_full())?,
                }
                let identity = m.identity.ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "enum member has no catalog identity"
                    )
                })?;
                write!(&mut self.manifest_output, " {}", identity.created_at())
                    .map_err(|_| manifest_full())?;
            }
            writeln!(&mut self.manifest_output).map_err(|_| manifest_full())?;
        }
        // Named composites precede tables because composite columns rebind by
        // catalog identity while the table definitions are restored.
        for (slot, definition) in storage.checkpoint_composites() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                definition.database,
            )?;
            use core::fmt::Write;
            let manifest_full = || {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "checkpoint manifest is full; raise checkpoint_manifest_bytes"
                )
            };
            write!(
                &mut self.manifest_output,
                "cmp2 {} {} {} {} {}",
                slot,
                definition.created_at,
                ManifestZeroName(definition.schema.as_str()),
                ManifestZeroName(definition.name.as_str()),
                definition.n_fields
            )
            .map_err(|_| manifest_full())?;
            for field in definition.fields() {
                let type_name = field
                    .user_type
                    .map(crate::storage::UserTypeName::storage_name);
                write!(
                    &mut self.manifest_output,
                    " {} {} {} {} {} {} {} {} {}",
                    ManifestZeroName(field.name.as_str()),
                    field.attribute_number,
                    u8::from(field.dropped),
                    u8::from(field.not_null),
                    field.ctype.code(),
                    field.type_mod,
                    field.collation.code(),
                    ManifestZeroName(
                        field
                            .user_type
                            .as_ref()
                            .map(|identity| identity.schema.as_str())
                            .unwrap_or("")
                    ),
                    ManifestZeroName(type_name.as_ref().map(|name| name.as_str()).unwrap_or(""))
                )
                .map_err(|_| manifest_full())?;
            }
            writeln!(&mut self.manifest_output).map_err(|_| manifest_full())?;
        }
        write_manifest(&mut self.manifest_output, "types_end")?;
        for (slot, method) in storage.checkpoint_access_methods() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                method.database,
            )?;
            use core::fmt::Write;
            let mut name = StackStr::<130>::new();
            for byte in method.definition.name.as_str().as_bytes() {
                let _ = write!(name, "{byte:02x}");
            }
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "am {} {} {} {}",
                    slot,
                    method.created_at,
                    name.as_str(),
                    method.definition.handler.code(),
                ),
            )?;
        }
        for slot in 0..storage.physical_table_count() {
            let table = storage.table(slot);
            let skip_table = {
                let definition = table.definition();
                !definition.identity().live()
                    || definition.persistence == crate::storage::RelationPersistence::Temporary
            };
            if skip_table {
                // A dropped table's recorded list must not linger into the
                // GC keep-set the swap below publishes.
                if slot < self.prev_scratch.len() {
                    self.prev_scratch[slot].clear();
                }
                continue;
            }
            let definitions = storage.table_definition_images();
            let definition = definitions.definition(storage, slot, 0)?;
            let identity = definitions
                .retained_identity(slot, 0)
                .expect("captured table identity");
            if !identity.live()
                || definition.persistence == crate::storage::RelationPersistence::Temporary
            {
                if slot < self.prev_scratch.len() {
                    self.prev_scratch[slot].clear();
                }
                continue;
            }
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                identity.database,
            )?;
            // Table + columns into the manifest.
            let mut table_line = StackStr::<278>::new();
            use core::fmt::Write as _;
            write!(
                table_line,
                "table2 {slot} {} {} {} {} {} {} {} {}",
                identity.created_at,
                definition.n_columns,
                u8::from(definition.has_toast),
                u8::from(definition.has_rules),
                match definition.kind {
                    crate::storage::TableKind::Local => 0,
                    crate::storage::TableKind::Foreign => 1,
                },
                definition.tablespace,
                match definition.access_method {
                    crate::storage::TableAccessMethod::Heap => 2,
                    crate::storage::TableAccessMethod::Catalog(oid) => oid.get(),
                },
                definition.inheritance.parents_ref().len(),
            )
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "table manifest line exceeds fixed capacity"
                )
            })?;
            for parent in definition.inheritance.parents_ref() {
                write!(table_line, " {parent}").map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "table manifest line exceeds fixed capacity"
                    )
                })?;
            }
            match definition.type_membership {
                crate::storage::TableTypeMembership::None => write!(table_line, " -1"),
                crate::storage::TableTypeMembership::Composite(reference) => {
                    write!(table_line, " {}", reference.storage_key())
                }
            }
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "table manifest line exceeds fixed capacity"
                )
            })?;
            write!(
                table_line,
                " {}",
                definition.storage_options.fillfactor.unwrap_or(0)
            )
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "table manifest line exceeds fixed capacity"
                )
            })?;
            write!(table_line, " {}", definition.name.as_str()).map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "table manifest line exceeds fixed capacity"
                )
            })?;
            write_manifest(&mut self.manifest_output, table_line.as_str())?;
            write_manifest(
                &mut self.manifest_output,
                format_args!("persist {}", definition.persistence.code() as char),
            )?;
            if !definition.attributes.validate(definition.n_columns) {
                return Err(sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "invalid column attribute layout"
                ));
            }
            write_manifest(
                &mut self.manifest_output,
                format_args!("attributes2 0 {}", definition.attributes.count),
            )?;
            for (number, attribute) in definition.attributes.slots
                [..usize::from(definition.attributes.count)]
                .iter()
                .enumerate()
            {
                use core::fmt::Write;
                let mut encoded = StackStr::<16>::new();
                for byte in attribute.encode() {
                    write!(encoded, "{byte:02x}").expect("fixed attribute encoding fits");
                }
                write_manifest(
                    &mut self.manifest_output,
                    format_args!("attributes2 {} {}", number + 1, encoded.as_str()),
                )?;
            }
            if definition.schema.as_str() != "public" {
                use core::fmt::Write;
                let mut hex = StackStr::<130>::new();
                for b in definition.schema.as_str().as_bytes() {
                    let _ = write!(hex, "{b:02x}");
                }
                write_manifest(
                    &mut self.manifest_output,
                    format_args!("tsch {}", hex.as_str()),
                )?;
            }
            write_partition_manifest(&mut self.manifest_output, definition.partition)?;
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "rls {} {} {}",
                    u8::from(definition.row_level_security.enabled),
                    u8::from(definition.row_level_security.forced),
                    definition.replica_identity.code(),
                ),
            )?;
            for (column, c) in definition.columns().iter().enumerate() {
                use core::fmt::Write as _;
                let default_value = c.default.constant().copied();
                let default_hex = default_to_hex(&default_value);
                // Non-constant DEFAULT text, hex-encoded (`0` sentinel = none),
                // placed before the name (which may itself contain spaces).
                let mut dexpr_hex = StackStr::<{ 2 * crate::storage::DEFAULT_EXPR_MAX + 1 }>::new();
                match c.default.expression() {
                    Some(e) => {
                        for b in e.as_str().as_bytes() {
                            let _ = write!(dexpr_hex, "{b:02x}");
                        }
                    }
                    None => {
                        let _ = write!(dexpr_hex, "0");
                    }
                }
                let flags = c.not_null.code()
                    | (u8::from(c.unique) << 2)
                    | (u8::from(c.primary) << 3)
                    | (u8::from(c.auto_increment) << 4)
                    | (u8::from(c.default.is_generated()) << 5)
                    | (u8::from(c.is_identity) << 6)
                    | (u8::from(c.identity_always) << 7);
                // The user-defined type name, hex-encoded (`0` = ordinary base type),
                // before the name (which may contain spaces).
                let mut domain_schema_hex = StackStr::<130>::new();
                match c.user_type {
                    Some(identity) => {
                        let schema = identity.schema;
                        for byte in schema.as_str().as_bytes() {
                            let _ = write!(domain_schema_hex, "{byte:02x}");
                        }
                    }
                    None => {
                        let _ = write!(domain_schema_hex, "0");
                    }
                }
                let mut domain_hex = StackStr::<256>::new();
                match c.user_type {
                    Some(identity) => {
                        for b in identity.storage_name().as_str().as_bytes() {
                            let _ = write!(domain_hex, "{b:02x}");
                        }
                    }
                    None => {
                        let _ = write!(domain_hex, "0");
                    }
                }
                let mut not_null_name_hex = StackStr::<128>::new();
                if let Some(name) = definition.not_null_names[column] {
                    for byte in name.as_str().as_bytes() {
                        let _ = write!(not_null_name_hex, "{byte:02x}");
                    }
                } else {
                    let _ = write!(not_null_name_hex, "0");
                }
                let mut column_name_hex = StackStr::<128>::new();
                for byte in c.name.as_str().as_bytes() {
                    let _ = write!(column_name_hex, "{byte:02x}");
                }
                write_manifest(
                    &mut self.manifest_output,
                    format_args!(
                        "col4 {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
                        c.ctype.code(),
                        flags,
                        u8::from(c.not_null_inheritable),
                        not_null_name_hex.as_str(),
                        c.type_mod,
                        default_hex.as_str(),
                        dexpr_hex.as_str(),
                        c.auto_increment_step,
                        c.statistics_target,
                        c.statistics_options.encoded().0,
                        c.statistics_options.encoded().1,
                        c.statistics_options.encoded().2,
                        c.storage.code(),
                        c.compression.code(),
                        c.collation.code(),
                        domain_schema_hex.as_str(),
                        domain_hex.as_str(),
                        column_name_hex.as_str()
                    ),
                )?;
            }
            let captured = self.checkpoint_statistics[slot]
                .as_ref()
                .expect("manifest captures statistics for every table slot");
            let table_statistics = captured.image();
            write_manifest(
                &mut self.manifest_output,
                format_args!("hidx {slot} {}", u8::from(captured.has_index())),
            )?;
            if table_statistics.valid {
                write_manifest(
                    &mut self.manifest_output,
                    format_args!(
                        "stats {slot} {} {} {}",
                        table_statistics.rows,
                        table_statistics.average_row_width,
                        table_statistics.analyzed_generation
                    ),
                )?;
                for (column, statistics) in table_statistics.columns[..definition.n_columns]
                    .iter()
                    .enumerate()
                {
                    if !statistics.valid {
                        continue;
                    }
                    write_manifest(
                        &mut self.manifest_output,
                        format_args!(
                            "cstat {slot} {column} {} {} {} {}",
                            statistics.null_fraction_ppm,
                            statistics.distinct_values,
                            statistics.distinct_fraction_ppm,
                            statistics.average_width
                        ),
                    )?;
                }
            }
            let serial_values = storage.table_serial_values(slot);
            for (ci, c) in definition.columns().iter().enumerate() {
                if c.auto_increment {
                    write_manifest(
                        &mut self.manifest_output,
                        format_args!("seq {ci} {}", serial_values[ci]),
                    )?;
                }
            }
            // Constraint lines (hex-encoded names/text tolerate spaces):
            // `ukey <kind> <timing> <ncols> <c0..cN> <hex-name>`
            for uk in definition.uniques() {
                use core::fmt::Write;
                let mut columns = StackStr::<64>::new();
                for c in uk.columns() {
                    let _ = write!(columns, "{c} ");
                }
                let mut hex_name = StackStr::<130>::new();
                for b in uk.name.as_str().as_bytes() {
                    let _ = write!(hex_name, "{b:02x}");
                }
                write_manifest(
                    &mut self.manifest_output,
                    format_args!(
                        "ukey {} {} {} {}{}",
                        uk.kind.code(),
                        uk.timing.code(),
                        uk.n_cols,
                        columns.as_str(),
                        hex_name.as_str()
                    ),
                )?;
            }
            // `chk <validation> <hex-name> <hex-predicate>`
            for check in definition.checks() {
                use core::fmt::Write;
                let mut hex_name = StackStr::<130>::new();
                for b in check.name.as_str().as_bytes() {
                    let _ = write!(hex_name, "{b:02x}");
                }
                let mut hexpr = StackStr::<{ 2 * crate::storage::CHECK_SQL_MAX }>::new();
                for b in check.expression.as_str().as_bytes() {
                    let _ = write!(hexpr, "{b:02x}");
                }
                write_manifest(
                    &mut self.manifest_output,
                    format_args!(
                        "chk {} {} {}",
                        check.validation.code(),
                        hex_name.as_str(),
                        hexpr.as_str()
                    ),
                )?;
            }
            // `fkey <ncols> <c..> <nparent> <p..> <actions> <timing> <validation> <names>`
            for fk in definition.fkeys() {
                use core::fmt::Write;
                let mut columns = StackStr::<64>::new();
                for c in fk.columns() {
                    let _ = write!(columns, "{c} ");
                }
                let mut pcols = StackStr::<64>::new();
                for c in fk.parent_cols() {
                    let _ = write!(pcols, "{c} ");
                }
                let mut hex_name = StackStr::<130>::new();
                for b in fk.name.as_str().as_bytes() {
                    let _ = write!(hex_name, "{b:02x}");
                }
                let mut hparent = StackStr::<130>::new();
                for b in fk.parent.as_str().as_bytes() {
                    let _ = write!(hparent, "{b:02x}");
                }
                let mut hparent_schema = StackStr::<130>::new();
                for b in fk.parent_schema.as_str().as_bytes() {
                    let _ = write!(hparent_schema, "{b:02x}");
                }
                write_manifest(
                    &mut self.manifest_output,
                    format_args!(
                        "fkey {} {}{} {}{} {} {} {} {} {} {}",
                        fk.n_cols,
                        columns.as_str(),
                        fk.n_parent_cols,
                        pcols.as_str(),
                        fk.on_delete.code(),
                        fk.on_update.code(),
                        fk.timing.code(),
                        fk.validation.code(),
                        hex_name.as_str(),
                        hparent.as_str(),
                        hparent_schema.as_str()
                    ),
                )?;
            }
            for exclusion in definition.exclusions() {
                use core::fmt::Write;
                let mut elements = StackStr::<128>::new();
                for position in 0..exclusion.n_cols {
                    let _ = write!(
                        elements,
                        "{} {} ",
                        exclusion.columns[position],
                        exclusion.operators[position].code()
                    );
                }
                let mut hex_name = StackStr::<130>::new();
                for byte in exclusion.name.as_str().as_bytes() {
                    let _ = write!(hex_name, "{byte:02x}");
                }
                let mut predicate =
                    StackStr::<{ 2 * crate::storage::EXCLUSION_PREDICATE_MAX }>::new();
                if let Some(source) = &exclusion.predicate {
                    for byte in source.as_str().as_bytes() {
                        let _ = write!(predicate, "{byte:02x}");
                    }
                } else {
                    let _ = write!(predicate, "-");
                }
                if elements.is_truncated() || hex_name.is_truncated() || predicate.is_truncated() {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "exclusion constraint manifest line exceeds its fixed buffer"
                    ));
                }
                write_manifest(
                    &mut self.manifest_output,
                    format_args!(
                        "excl {} {} {}{} {}",
                        exclusion.timing.code(),
                        exclusion.n_cols,
                        elements.as_str(),
                        hex_name.as_str(),
                        predicate.as_str()
                    ),
                )?;
            }

            for (position, identity) in definition.implicit_index_identities().enumerate() {
                let identity = identity.ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "checkpoint implicit index has no creation identity"
                    )
                })?;
                write_manifest(
                    &mut self.manifest_output,
                    format_args!("implicit_index {position} {}", identity.created_at()),
                )?;
            }

            if slot != storage.large_object_page_table() {
                for (position, identity) in definition.constraint_identities().enumerate() {
                    let identity = identity.ok_or_else(|| {
                        sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "checkpoint constraint has no creation identity"
                        )
                    })?;
                    write_manifest(
                        &mut self.manifest_output,
                        format_args!("constraint_identity {position} {}", identity.created_at()),
                    )?;
                }
            }

            // A slot not sliced this sweep carries its published list
            // forward untouched — the table is clean, so today's list is
            // yesterday's. A sliced slot's list was recorded by its beat.
            if !self.sliced_this_sweep.get(slot).copied().unwrap_or(false)
                && slot < self.prev_scratch.len()
            {
                self.prev_scratch[slot].copy_from(&self.prev_ssts[slot]);
            }
            // A merge finished since the last publish composes here: its
            // pair still present at its position (a delta only appends at
            // the tail, so positions are stable under it) means the merged
            // member replaces the two; a collapse superseded it, and the
            // merged blocks simply sweep as orphans. Recomputed from the
            // carried base on every attempt, so a publish retried after a
            // mid-CAS failure applies it exactly once.
            let completed = self.merge_done[slot].as_ref().and_then(|done| {
                (pair_at(&self.prev_scratch[slot], done.at)
                    == Some((done.old0.handle, done.old1.handle)))
                .then_some((done.at, done.merged, (done.old0.handle, done.old1.handle)))
            });
            if let Some((at, merged, expected)) = completed {
                let target = storage.capture_spill_publication(
                    slot,
                    storage.table(slot).created_at(),
                    storage.table(slot).rewrite_generation(),
                )?;
                self.prev_scratch[slot].replace_pair(at, merged);
                self.pending_installs
                    .retain(|(s, i)| !(*s == slot && matches!(i, SlotInstall::MergePair { .. })));
                self.pending_installs.push((
                    slot,
                    SlotInstall::MergePair {
                        target,
                        expected,
                        at,
                        handle: merged.map(|member| member.handle),
                    },
                ));
            }
            for (idx, p) in self.prev_scratch[slot].iter().enumerate() {
                let h = p.handle;
                let (mut ih, mut fh, mut rh) = ([0u8; 64], [0u8; 64], [0u8; 64]);
                h.index.write_key(&mut ih);
                h.filter.write_key(&mut fh);
                h.roster.write_key(&mut rh);
                write_manifest(
                    &mut self.manifest_output,
                    format_args!(
                        "dsst {slot} {idx} {} {} {} {} {} {}",
                        p.count,
                        p.crc,
                        core::str::from_utf8(&ih).expect("hex"),
                        core::str::from_utf8(&fh).expect("hex"),
                        core::str::from_utf8(&rh).expect("hex"),
                        h.format.manifest_id(),
                    ),
                )?;
            }
            if self.prev_scratch[slot].n == 0 {
                // An empty table still records its (zero-row) state so the
                // loader creates it.
                write_manifest(
                    &mut self.manifest_output,
                    format_args!("dsst {slot} 0 0 0 - - -"),
                )?;
            }
            for binding in 0..storage.value_binding_count(slot) {
                let (columns, n_columns) = storage.value_binding_columns(slot, binding);
                let index_created_at = storage.value_binding_created_at(slot, binding);
                let include_mask = storage.value_binding_include_mask(slot, binding);
                let handle = self
                    .pending_value_installs
                    .iter()
                    .find(|install| {
                        install.slot == slot
                            && install.table_created_at == storage.table(slot).created_at()
                            && install.n_columns == n_columns
                            && install.columns[..n_columns] == columns[..n_columns]
                            && install.index_created_at == index_created_at
                            && install.include_mask == include_mask
                    })
                    .and_then(|install| install.handle)
                    .or_else(|| storage.value_binding_handle(slot, binding));
                let Some(handle) = handle else { continue };
                use core::fmt::Write;
                let mut column_text = StackStr::<128>::new();
                for column in &columns[..n_columns] {
                    let _ = write!(column_text, "{column} ");
                }
                let mut roster = [0u8; 64];
                handle.roster.write_key(&mut roster);
                write_manifest(
                    &mut self.manifest_output,
                    format_args!(
                        "vix {slot} {n_columns} {}{include_mask} {} {} {} {}",
                        column_text.as_str(),
                        index_created_at.map_or_else(
                            || StackStr::<32>::from_str("-"),
                            |identity| stack_format!(32, "{identity}"),
                        ),
                        core::str::from_utf8(&roster).expect("hex"),
                        handle.entries,
                        handle.published_lsn,
                    ),
                )?;
            }
        }
        for (statistics_slot, statistics) in storage.checkpoint_extended_statistics() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                statistics.database,
            )?;
            use core::fmt::Write as _;
            let mutable = statistics.definition_for(0);
            let mut schema_hex = StackStr::<130>::new();
            let mut name_hex = StackStr::<130>::new();
            for byte in mutable.schema.as_str().as_bytes() {
                let _ = write!(schema_hex, "{byte:02x}");
            }
            for byte in mutable.name.as_str().as_bytes() {
                let _ = write!(name_hex, "{byte:02x}");
            }
            let mut line = StackStr::<10000>::new();
            let _ = write!(
                line,
                "estat {} {} {} {} {} {} {}",
                statistics.created_at,
                statistics.table,
                mutable.target.map_or(-1i32, i32::from),
                statistics.kinds.code(),
                u8::from(statistics.expression_only),
                statistics.n_keys,
                schema_hex.as_str(),
            );
            let _ = write!(line, " {}", name_hex.as_str());
            for key in statistics.keys_for(0) {
                match key {
                    crate::storage::ExtendedStatisticsKey::Column(column) => {
                        let _ = line.write_str(" c");
                        for byte in column.as_str().as_bytes() {
                            let _ = write!(line, "{byte:02x}");
                        }
                    }
                    crate::storage::ExtendedStatisticsKey::Expression(expression) => {
                        let _ = line.write_str(" e");
                        for byte in expression.as_str().as_bytes() {
                            let _ = write!(line, "{byte:02x}");
                        }
                    }
                }
            }
            if line.is_truncated() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "extended-statistics manifest line exceeds its fixed buffer"
                ));
            }
            write_manifest(&mut self.manifest_output, format_args!("{}", line.as_str()))?;

            let data = storage.extended_statistics_data(statistics_slot, 0);
            if !data.valid {
                continue;
            }
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "estatdata {} {} {} {} {} {}",
                    statistics.created_at,
                    u8::from(data.inherited),
                    data.analyzed_generation,
                    data.rows,
                    data.non_null_rows,
                    data.distinct_values,
                ),
            )?;
            for (index, strength) in data.dependencies_ppm.iter().copied().enumerate() {
                if strength != 0 {
                    write_manifest(
                        &mut self.manifest_output,
                        format_args!("estatdep {} {index} {strength}", statistics.created_at),
                    )?;
                }
            }
            for (key, column) in data.expression_statistics.iter().copied().enumerate() {
                if column.valid {
                    write_manifest(
                        &mut self.manifest_output,
                        format_args!(
                            "estatexpr {} {key} {} {} {} {}",
                            statistics.created_at,
                            column.null_fraction_ppm,
                            column.distinct_values,
                            column.distinct_fraction_ppm,
                            column.average_width,
                        ),
                    )?;
                }
            }
            for entry in &data.mcv[..usize::from(data.n_mcv)] {
                let mut value_hex =
                    StackStr::<{ 2 * crate::storage::EXTENDED_STATISTICS_MCV_TEXT_MAX }>::new();
                for byte in entry.values.as_str().as_bytes() {
                    let _ = write!(value_hex, "{byte:02x}");
                }
                write_manifest(
                    &mut self.manifest_output,
                    format_args!(
                        "estatmcv {} {} {} {}",
                        statistics.created_at,
                        entry.hash,
                        entry.count,
                        value_hex.as_str(),
                    ),
                )?;
                if entry.base_frequency_bits != 0 {
                    write_manifest(
                        &mut self.manifest_output,
                        format_args!(
                            "estatmcvbase {} {} {}",
                            statistics.created_at, entry.hash, entry.base_frequency_bits,
                        ),
                    )?;
                }
            }
        }
        for (slot, entry) in storage.checkpoint_foreign_wrappers() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                entry.database,
            )?;
            use core::fmt::Write as _;
            let definition = entry.definition;
            let owner = storage.role_name(entry.ownership.owner as usize, 0);
            let mut line = StackStr::<12_288>::new();
            let _ = write!(
                line,
                "fdw {} {} {} {} {} {} {}",
                slot,
                entry.created_at,
                manifest_hex::<130>(owner.as_str()),
                match definition.handler {
                    crate::storage::foreign::ForeignDataHandler::None => 0,
                    crate::storage::foreign::ForeignDataHandler::Postgres => 1,
                },
                match definition.validator {
                    crate::storage::foreign::ForeignDataValidator::None => 0,
                    crate::storage::foreign::ForeignDataValidator::Postgres => 1,
                },
                manifest_hex::<130>(definition.name.as_str()),
                definition.options.entries().len(),
            );
            for option in definition.options.entries() {
                let _ = write!(
                    line,
                    " {} {}",
                    manifest_hex::<130>(option.name.as_str()),
                    manifest_hex::<{ 2 * crate::storage::foreign::FOREIGN_OPTION_VALUE_MAX + 1 }>(
                        option.value.as_str()
                    ),
                );
            }
            write_manifest(&mut self.manifest_output, line.as_str())?;
        }
        for (slot, entry) in storage.checkpoint_foreign_servers() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                entry.database,
            )?;
            use core::fmt::Write as _;
            let definition = entry.definition;
            let owner = storage.role_name(entry.ownership.owner as usize, 0);
            let wrapper = storage.checkpoint_foreign_wrapper(definition.wrapper as usize);
            let mut line = StackStr::<12_288>::new();
            let _ = write!(
                line,
                "fsrv {} {} {} {} {} {} {} {}",
                slot,
                entry.created_at,
                manifest_hex::<130>(owner.as_str()),
                manifest_hex::<130>(wrapper.definition.name.as_str()),
                manifest_optional_hex(definition.server_type),
                manifest_optional_hex(definition.version),
                manifest_hex::<130>(definition.name.as_str()),
                definition.options.entries().len(),
            );
            for option in definition.options.entries() {
                let _ = write!(
                    line,
                    " {} {}",
                    manifest_hex::<130>(option.name.as_str()),
                    manifest_hex::<{ 2 * crate::storage::foreign::FOREIGN_OPTION_VALUE_MAX + 1 }>(
                        option.value.as_str()
                    ),
                );
            }
            write_manifest(&mut self.manifest_output, line.as_str())?;
        }
        for (slot, entry) in storage.checkpoint_foreign_user_mappings() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                entry.database,
            )?;
            use core::fmt::Write as _;
            let definition = entry.definition;
            let server = storage.checkpoint_foreign_server(definition.server as usize);
            let (user_kind, user_name) = match definition.user {
                crate::storage::foreign::ForeignMappingUser::Public => (0, StackStr::from_str("-")),
                crate::storage::foreign::ForeignMappingUser::Role(slot) => (
                    1,
                    manifest_hex::<130>(storage.role_name(slot as usize, 0).as_str()),
                ),
            };
            let mut line = StackStr::<12_288>::new();
            let _ = write!(
                line,
                "fum {} {} {} {} {} {}",
                slot,
                entry.created_at,
                manifest_hex::<130>(server.definition.name.as_str()),
                user_kind,
                user_name,
                definition.options.entries().len(),
            );
            for option in definition.options.entries() {
                let _ = write!(
                    line,
                    " {} {}",
                    manifest_hex::<130>(option.name.as_str()),
                    manifest_hex::<{ 2 * crate::storage::foreign::FOREIGN_OPTION_VALUE_MAX + 1 }>(
                        option.value.as_str()
                    ),
                );
            }
            write_manifest(&mut self.manifest_output, line.as_str())?;
        }
        for (slot, entry) in storage.checkpoint_foreign_tables() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                entry.database,
            )?;
            use core::fmt::Write as _;
            let definition = entry.definition;
            let table = storage.table_def(definition.table as usize, 0);
            let server = storage.checkpoint_foreign_server(definition.server as usize);
            let mut line = StackStr::<40_960>::new();
            let _ = write!(
                line,
                "ftab {} {} {} {} {} {}",
                slot,
                entry.created_at,
                manifest_hex::<130>(table.schema.as_str()),
                manifest_hex::<130>(table.name.as_str()),
                manifest_hex::<130>(server.definition.name.as_str()),
                definition.options.entries().len(),
            );
            for option in definition.options.entries() {
                let _ = write!(
                    line,
                    " {} {}",
                    manifest_hex::<130>(option.name.as_str()),
                    manifest_hex::<{ 2 * crate::storage::foreign::FOREIGN_OPTION_VALUE_MAX + 1 }>(
                        option.value.as_str()
                    ),
                );
            }
            let _ = write!(line, " {}", definition.column_options.entries().len());
            for column in definition.column_options.entries() {
                let _ = write!(
                    line,
                    " {} {} {}",
                    column.column,
                    manifest_hex::<130>(column.option.name.as_str()),
                    manifest_hex::<{ 2 * crate::storage::foreign::FOREIGN_OPTION_VALUE_MAX + 1 }>(
                        column.option.value.as_str()
                    ),
                );
            }
            write_manifest(&mut self.manifest_output, line.as_str())?;
        }
        // View SQL and names are hex because the manifest is space-separated.
        for (view_slot, view) in storage.checkpoint_views() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                view.database,
            )?;
            use core::fmt::Write;
            let mut hex = StackStr::<{ 2 * crate::storage::VIEW_SQL_MAX }>::new();
            for b in storage.view_sql(view_slot).as_str().as_bytes() {
                let _ = write!(hex, "{b:02x}");
            }
            let mut hschema = StackStr::<130>::new();
            for b in view.schema.as_str().as_bytes() {
                let _ = write!(hschema, "{b:02x}");
            }
            let mut hpath = StackStr::<260>::new();
            for b in storage.view_creation_path(view_slot).as_str().as_bytes() {
                let _ = write!(hpath, "{b:02x}");
            }
            let mut hname = StackStr::<130>::new();
            for b in view.name.as_str().as_bytes() {
                let _ = write!(hname, "{b:02x}");
            }
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "vw13 {} {} {} {} {} {} {} {} {} {} {} {} {}",
                    view_slot,
                    view.created_at,
                    view.return_rule,
                    storage.rule(usize::from(view.return_rule)).created_at,
                    hex.as_str(),
                    hschema.as_str(),
                    hpath.as_str(),
                    hname.as_str(),
                    u8::from(matches!(
                        view.options.security,
                        crate::storage::ViewSecurity::Invoker
                    )),
                    view.options.security_barrier.code(),
                    view.options
                        .check_option
                        .map_or(0, crate::storage::ViewCheckOption::code),
                    ManifestViewColumns(view.columns),
                    ManifestDependencies(storage.view_dependencies(view_slot, 0).view())
                ),
            )?;
        }
        // Materialized views persist their backing relation separately.
        // Publications: database-scoped names plus explicit table slots.
        for (_, publication) in storage.checkpoint_publications() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                publication.database,
            )?;
            use core::fmt::Write;
            let mut name = StackStr::<130>::new();
            for byte in publication.name.as_str().as_bytes() {
                let _ = write!(name, "{byte:02x}");
            }
            let flags = u8::from(publication.all_tables)
                | (u8::from(publication.publish_insert) << 1)
                | (u8::from(publication.publish_update) << 2)
                | (u8::from(publication.publish_delete) << 3)
                | (u8::from(publication.publish_truncate) << 4);
            let flags = flags | (u8::from(publication.publish_via_partition_root) << 5);
            let flags = flags
                | (u8::from(matches!(
                    publication.publish_generated_columns,
                    crate::storage::PublishGeneratedColumns::Stored
                )) << 6);
            write!(
                &mut self.manifest_output,
                "pub {} {} {} {} {} {}",
                name.as_str(),
                publication.ownership.owner,
                publication.created_at,
                flags,
                publication.table_count,
                publication.schema_count
            )
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "checkpoint manifest is full; raise checkpoint_manifest_bytes"
                )
            })?;
            for (index, ((table, mask), descendants)) in publication.tables
                [..publication.table_count]
                .iter()
                .zip(&publication.table_column_masks[..publication.table_count])
                .zip(&publication.table_include_descendants[..publication.table_count])
                .enumerate()
            {
                let filter = publication.table_filters.get(index);
                write!(
                    &mut self.manifest_output,
                    " {table} {mask} {} ",
                    u8::from(*descendants)
                )
                .map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "checkpoint manifest is full; raise checkpoint_manifest_bytes"
                    )
                })?;
                if filter.is_empty() {
                    write!(&mut self.manifest_output, "-").map_err(|_| {
                        sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "checkpoint manifest is full; raise checkpoint_manifest_bytes"
                        )
                    })?;
                } else {
                    for byte in filter.as_bytes() {
                        write!(&mut self.manifest_output, "{byte:02x}").map_err(|_| {
                            sql_err!(
                                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                "checkpoint manifest is full; raise checkpoint_manifest_bytes"
                            )
                        })?;
                    }
                }
            }
            for schema in &publication.schemas[..publication.schema_count] {
                write!(&mut self.manifest_output, " {schema}").map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "checkpoint manifest is full; raise checkpoint_manifest_bytes"
                    )
                })?;
            }
            writeln!(&mut self.manifest_output).map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "checkpoint manifest is full; raise checkpoint_manifest_bytes"
                )
            })?;
        }
        // A replication slot's active flag is process-local; only its resume
        // positions survive a checkpoint and restart.
        for (_, slot) in storage.checkpoint_replication_slots() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                slot.database,
            )?;
            use core::fmt::Write;
            let mut name = StackStr::<130>::new();
            for byte in slot.name.as_str().as_bytes() {
                let _ = write!(name, "{byte:02x}");
            }
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "rslot {} {} {} {}",
                    name.as_str(),
                    slot.restart_lsn,
                    slot.confirmed_flush_lsn,
                    slot.behavior.code(),
                ),
            )?;
        }
        // Subscriptions are catalog state, not a local worker cache. Hex
        // fields preserve conninfo whitespace in the line-oriented manifest.
        for (_, subscription) in storage.checkpoint_subscriptions() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                subscription.database,
            )?;
            use core::fmt::Write;
            let mut name = StackStr::<130>::new();
            let mut connection =
                StackStr::<{ 2 * crate::storage::SUBSCRIPTION_CONNINFO_BYTES }>::new();
            let mut slot_name = StackStr::<130>::new();
            for byte in subscription.name.as_str().as_bytes() {
                let _ = write!(name, "{byte:02x}");
            }
            for byte in subscription.connection.as_str().as_bytes() {
                let _ = write!(connection, "{byte:02x}");
            }
            let slot_kind = match subscription.slot {
                crate::storage::SubscriptionSlot::Absent => 0,
                crate::storage::SubscriptionSlot::External(slot) => {
                    for byte in slot.as_str().as_bytes() {
                        let _ = write!(slot_name, "{byte:02x}");
                    }
                    1
                }
                crate::storage::SubscriptionSlot::Managed(slot) => {
                    for byte in slot.as_str().as_bytes() {
                        let _ = write!(slot_name, "{byte:02x}");
                    }
                    2
                }
            };
            if slot_kind == 0 {
                let _ = write!(slot_name, "-");
            }
            let mut publications =
                StackStr::<{ crate::storage::MAX_SUBSCRIPTION_PUBLICATIONS * 130 }>::new();
            for publication in &subscription.publications[..subscription.publication_count] {
                let _ = write!(publications, " ");
                for byte in publication.as_str().as_bytes() {
                    let _ = write!(publications, "{byte:02x}");
                }
            }
            let mut failure_message = StackStr::<384>::new();
            let failure_code = subscription.failure.map_or_else(
                || StackStr::<5>::from_str("-"),
                |failure| StackStr::<5>::from_str(failure.sqlstate.as_str()),
            );
            if let Some(failure) = subscription.failure {
                for byte in failure.message.as_str().as_bytes() {
                    let _ = write!(failure_message, "{byte:02x}");
                }
            } else {
                let _ = write!(failure_message, "-");
            }
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "sub {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}{}",
                    name.as_str(),
                    subscription.ownership.owner,
                    u8::from(subscription.enabled),
                    connection.as_str(),
                    slot_kind,
                    slot_name.as_str(),
                    subscription.bootstrap.code(),
                    u8::from(subscription.behavior.binary),
                    subscription.behavior.streaming.code(),
                    subscription.behavior.synchronous_commit.code(),
                    u8::from(subscription.behavior.two_phase),
                    u8::from(subscription.behavior.disable_on_error),
                    u8::from(subscription.behavior.password_required),
                    u8::from(subscription.behavior.run_as_owner),
                    subscription.behavior.origin.code(),
                    u8::from(subscription.behavior.failover),
                    u8::from(subscription.behavior.skip_lsn.is_some()),
                    subscription.behavior.skip_lsn.unwrap_or(0),
                    subscription.created_at,
                    subscription.definition_generation,
                    subscription.confirmed_lsn,
                    subscription.origin_lsn,
                    u8::from(
                        subscription.cleanup
                            == crate::storage::SubscriptionCleanup::DropManagedSlot
                    ),
                    u8::from(subscription.failure.is_some()),
                    failure_code.as_str(),
                    failure_message.as_str(),
                    subscription.publication_count,
                    publications.as_str(),
                ),
            )?;
            for relation in storage.subscription_relations_visible_to(&subscription, 0) {
                let table = storage.table_def(relation.table_slot(), 0);
                let mut schema = StackStr::<130>::new();
                let mut table_name = StackStr::<130>::new();
                for byte in table.schema.as_str().as_bytes() {
                    let _ = write!(schema, "{byte:02x}");
                }
                for byte in table.name.as_str().as_bytes() {
                    let _ = write!(table_name, "{byte:02x}");
                }
                write_manifest(
                    &mut self.manifest_output,
                    format_args!(
                        "subrel {} {} {} {} {} {}",
                        subscription.created_at,
                        subscription.definition_generation,
                        schema.as_str(),
                        table_name.as_str(),
                        relation.state().code(),
                        relation.synchronization_lsn(),
                    ),
                )?;
            }
        }
        // The backing table's rows serialize through the ordinary table/dsst
        // loop; this line records only the defining query.
        for (matview_slot, mv) in storage.checkpoint_matviews() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                mv.database,
            )?;
            let backing = storage.table_def(storage.matview_table(matview_slot), 0);
            use core::fmt::Write;
            let mut hex = StackStr::<{ 2 * crate::storage::VIEW_SQL_MAX }>::new();
            for b in mv.sql.as_str().as_bytes() {
                let _ = write!(hex, "{b:02x}");
            }
            let mut hschema = StackStr::<130>::new();
            for b in backing.schema.as_str().as_bytes() {
                let _ = write!(hschema, "{b:02x}");
            }
            let mut hpath = StackStr::<260>::new();
            for b in mv.creation_path.as_str().as_bytes() {
                let _ = write!(hpath, "{b:02x}");
            }
            let mut hname = StackStr::<130>::new();
            for b in backing.name.as_str().as_bytes() {
                let _ = write!(hname, "{b:02x}");
            }
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "mv6 {} {} {} {} {} {} {} {} {} {}",
                    matview_slot,
                    mv.created_at,
                    mv.backing_table,
                    storage.table(usize::from(mv.backing_table)).created_at(),
                    hex.as_str(),
                    hschema.as_str(),
                    hpath.as_str(),
                    hname.as_str(),
                    u8::from(mv.populated),
                    ManifestDependencies(storage.matview_dependencies(matview_slot, 0).view())
                ),
            )?;
        }
        // Sequences: hex schema/name, then the numeric parameters and the live
        // value state. A sequence stores no rows, so
        // this line is its whole durable form.
        for (sequence_slot, seq, last_value, is_called, log_count) in
            storage.checkpoint_sequences_with_slots()
        {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                seq.database,
            )?;
            use core::fmt::Write;
            let mut hschema = StackStr::<130>::new();
            for b in seq.schema.as_str().as_bytes() {
                let _ = write!(hschema, "{b:02x}");
            }
            let mut hname = StackStr::<130>::new();
            for b in seq.name.as_str().as_bytes() {
                let _ = write!(hname, "{b:02x}");
            }
            let mut owner_schema = StackStr::<130>::new();
            let mut owner_table = StackStr::<130>::new();
            let mut owner_column = StackStr::<130>::new();
            if let Some(owner) = seq.owner {
                for byte in owner.table_schema.as_str().as_bytes() {
                    let _ = write!(owner_schema, "{byte:02x}");
                }
                for byte in owner.table.as_str().as_bytes() {
                    let _ = write!(owner_table, "{byte:02x}");
                }
                for byte in owner.column.as_str().as_bytes() {
                    let _ = write!(owner_column, "{byte:02x}");
                }
            } else {
                let _ = write!(owner_schema, "0");
                let _ = write!(owner_table, "0");
                let _ = write!(owner_column, "0");
            }
            let mut generator_schema = StackStr::<130>::new();
            let mut generator_table = StackStr::<130>::new();
            let mut generator_column = StackStr::<130>::new();
            if let Some(generator) = seq.generator_for {
                for byte in generator.table_schema.as_str().as_bytes() {
                    let _ = write!(generator_schema, "{byte:02x}");
                }
                for byte in generator.table.as_str().as_bytes() {
                    let _ = write!(generator_table, "{byte:02x}");
                }
                for byte in generator.column.as_str().as_bytes() {
                    let _ = write!(generator_column, "{byte:02x}");
                }
            } else {
                let _ = write!(generator_schema, "0");
                let _ = write!(generator_table, "0");
                let _ = write!(generator_column, "0");
            }
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "sq7 {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
                    sequence_slot,
                    seq.created_at,
                    hschema.as_str(),
                    hname.as_str(),
                    seq.persistence.code() as char,
                    seq.data_type.to_u8(),
                    seq.increment,
                    seq.min_value,
                    seq.max_value,
                    seq.start_value,
                    seq.cache,
                    u8::from(seq.cycle),
                    last_value,
                    u8::from(is_called),
                    log_count,
                    owner_schema.as_str(),
                    owner_table.as_str(),
                    owner_column.as_str(),
                    generator_schema.as_str(),
                    generator_table.as_str(),
                    generator_column.as_str(),
                ),
            )?;
        }
        for slot in 0..storage.routine_count() {
            let routine = storage.routine(slot);
            if !routine.visible_to(0) {
                continue;
            }
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                routine.database,
            )?;
            use core::fmt::Write;
            let mut owner = StackStr::<130>::new();
            let mut schema = StackStr::<130>::new();
            let mut name = StackStr::<130>::new();
            let mut body = StackStr::<{ 2 * crate::storage::ROUTINE_SQL_MAX }>::new();
            let mut creation_path = StackStr::<260>::new();
            for byte in storage
                .role(routine.ownership.owner_to(0) as usize)
                .name
                .as_str()
                .as_bytes()
            {
                let _ = write!(owner, "{byte:02x}");
            }
            for byte in routine.schema.as_str().as_bytes() {
                let _ = write!(schema, "{byte:02x}");
            }
            for byte in routine.name.as_str().as_bytes() {
                let _ = write!(name, "{byte:02x}");
            }
            for byte in routine.creation_path.as_str().as_bytes() {
                let _ = write!(creation_path, "{byte:02x}");
            }
            if creation_path.as_str().is_empty() {
                let _ = creation_path.write_str("-");
            }
            let aggregate_body = match routine.kind {
                crate::storage::RoutineKind::Aggregate(aggregate) => Some(aggregate.encode_wire()),
                _ => None,
            };
            let serialized_body = aggregate_body
                .as_ref()
                .map_or(routine.body.as_str(), |body| body.as_str());
            for byte in serialized_body.as_bytes() {
                let _ = write!(body, "{byte:02x}");
            }
            let result_identity = match routine.kind {
                crate::storage::RoutineKind::Function { result }
                | crate::storage::RoutineKind::SetFunction { result } => result.user_type,
                crate::storage::RoutineKind::Aggregate(aggregate) => {
                    aggregate.result_type.user_type
                }
                _ => None,
            };
            let mut result_schema = StackStr::<130>::new();
            let mut result_name = StackStr::<130>::new();
            if let Some(identity) = result_identity {
                for byte in identity.schema.as_str().as_bytes() {
                    let _ = write!(result_schema, "{byte:02x}");
                }
                for byte in identity.storage_name().as_str().as_bytes() {
                    let _ = write!(result_name, "{byte:02x}");
                }
            } else {
                let _ = write!(result_schema, "-");
                let _ = write!(result_name, "-");
            }
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "rtn2 {} {} {} {} {} {} {} {}{}{}{} {} {} {} {} {} {} {} {} {} {} {} {}{} {} {}",
                    slot,
                    routine.created_at,
                    owner.as_str(),
                    match routine.kind {
                        crate::storage::RoutineKind::Function { result }
                        | crate::storage::RoutineKind::SetFunction { result } => result.ctype,
                        crate::storage::RoutineKind::Aggregate(aggregate) => {
                            aggregate.result_type.ctype
                        }
                        crate::storage::RoutineKind::TableFunction
                        | crate::storage::RoutineKind::RecordFunction { .. }
                        | crate::storage::RoutineKind::Trigger
                        | crate::storage::RoutineKind::EventTrigger
                        | crate::storage::RoutineKind::Procedure => ColType::Text,
                    }
                    .code(),
                    routine.argument_count,
                    schema.as_str(),
                    name.as_str(),
                    body.as_str(),
                    ManifestRoutineArguments(routine.arguments()),
                    ManifestRoutineParameters(routine.parameters()),
                    ManifestRoutineConfigs(routine.configs()),
                    u8::from(routine.attributes.strict),
                    routine.attributes.volatility.code(),
                    routine.attributes.parallel.code(),
                    routine.body_kind.code(),
                    routine.language.code(),
                    u8::from(routine.attributes.security_definer),
                    u8::from(routine.attributes.leakproof),
                    routine.attributes.cost_bits.unwrap_or(0),
                    routine.attributes.rows_bits.unwrap_or(0),
                    routine.kind.wire_code(),
                    result_schema.as_str(),
                    result_name.as_str(),
                    ManifestRoutineColumns(&routine),
                    creation_path.as_str(),
                    ManifestDependencies(storage.routine_dependencies_for(slot, 0).view()),
                ),
            )?;
        }
        for (slot, event_trigger) in storage.checkpoint_event_triggers() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                event_trigger.database,
            )?;
            use core::fmt::Write as _;
            let definition = event_trigger.definition;
            let routine = storage.routine(usize::from(definition.function));
            let owner = storage.role(usize::from(definition.ownership.owner_to(0)));
            let mut tags = StackStr::<{ crate::storage::MAX_EVENT_TRIGGER_TAGS * 132 + 4 }>::new();
            let _ = write!(tags, "{}", definition.tags.values().len());
            for tag in definition.tags.values() {
                let _ = write!(tags, " {}", ManifestName(tag.as_str()));
            }
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "evt {} {} {} {} {} {} {} {} {}",
                    slot,
                    event_trigger.created_at,
                    ManifestName(definition.name.as_str()),
                    definition.event.code(),
                    ManifestName(routine.schema.as_str()),
                    ManifestName(routine.name.as_str()),
                    definition.enabled.code(),
                    ManifestName(owner.name.as_str()),
                    tags.as_str(),
                ),
            )?;
        }
        for (slot, rule) in storage.checkpoint_rules() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                rule.database,
            )?;
            use core::fmt::Write as _;
            let definition = rule.definition;
            let (target, schema, relation) = match definition.target {
                crate::storage::RuleTarget::Table(target) => {
                    let table = storage.table_def(usize::from(target), 0);
                    (0u8, table.schema, table.name)
                }
                crate::storage::RuleTarget::View(target) => {
                    let view = storage.view(usize::from(target));
                    (1u8, view.schema, view.name)
                }
            };
            let condition = definition
                .condition
                .unwrap_or(crate::storage::RuleTextSpan {
                    start: u16::MAX,
                    len: 0,
                });
            let mut spans = StackStr::<512>::new();
            let _ = write!(spans, "{}", definition.action_count);
            for action in &definition.actions[..usize::from(definition.action_count)] {
                let _ = write!(spans, " {} {}", action.start, action.len);
            }
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "rul {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
                    slot,
                    rule.created_at,
                    target,
                    ManifestName(schema.as_str()),
                    ManifestName(relation.as_str()),
                    ManifestName(definition.name.as_str()),
                    definition.event as u8,
                    definition.mode as u8,
                    definition.enabled.code(),
                    ManifestName(definition.source.as_str()),
                    condition.start,
                    condition.len,
                    spans.as_str(),
                    definition.returning_action.map_or(u16::MAX, u16::from),
                    ManifestName(definition.creation_path.as_str()),
                    ManifestDependencies(storage.rule_dependencies(slot, 0).view()),
                ),
            )?;
        }
        for (slot, cast) in storage.checkpoint_casts() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                cast.database,
            )?;
            let function = match cast.method {
                crate::storage::CastMethod::Function(oid) => oid,
                crate::storage::CastMethod::Binary | crate::storage::CastMethod::InOut => 0,
            };
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "cst {} {} {} {} {} {} {}",
                    slot,
                    cast.created_at,
                    ManifestRoutineResult(cast.source),
                    ManifestRoutineResult(cast.target),
                    cast.method.code(),
                    function,
                    cast.context.code(),
                ),
            )?;
        }
        for (slot, collation) in storage.checkpoint_collations() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                collation.database,
            )?;
            let definition = collation.definition;
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "coll {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
                    slot,
                    collation.created_at,
                    ManifestName(definition.schema.as_str()),
                    ManifestName(definition.name.as_str()),
                    definition.owner,
                    definition.provider as u8,
                    u8::from(definition.deterministic),
                    definition.encoding.map_or(255, |encoding| encoding.code()),
                    ManifestName(definition.collate.as_str()),
                    ManifestName(definition.ctype.as_str()),
                    ManifestName(definition.locale.as_str()),
                    ManifestName(definition.rules.as_str()),
                    ManifestName(definition.version.as_str()),
                    match definition.behavior {
                        crate::storage::CollationBehavior::Bytewise => 0,
                        crate::storage::CollationBehavior::Database => 1,
                    },
                ),
            )?;
        }
        for (slot, object) in storage.checkpoint_text_search_objects() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                object.database,
            )?;
            let definition = object.definition;
            match definition {
                crate::storage::TextSearchDefinition::Parser {
                    schema,
                    name,
                    oid,
                    start,
                    gettoken,
                    end,
                    headline,
                    lextypes,
                } => write_manifest(
                    &mut self.manifest_output,
                    format_args!(
                        "tsobj {} {} p {} {} {} {} {} {} {} {}",
                        slot,
                        object.created_at,
                        ManifestName(schema.as_str()),
                        ManifestName(name.as_str()),
                        oid,
                        start,
                        gettoken,
                        end,
                        headline,
                        lextypes,
                    ),
                )?,
                crate::storage::TextSearchDefinition::Template {
                    schema,
                    name,
                    oid,
                    init,
                    lexize,
                    behavior,
                } => write_manifest(
                    &mut self.manifest_output,
                    format_args!(
                        "tsobj {} {} t {} {} {} {} {} {}",
                        slot,
                        object.created_at,
                        ManifestName(schema.as_str()),
                        ManifestName(name.as_str()),
                        oid,
                        init,
                        lexize,
                        text_search_behavior_code(behavior),
                    ),
                )?,
                crate::storage::TextSearchDefinition::Dictionary {
                    schema,
                    name,
                    oid,
                    owner,
                    template,
                    options,
                    behavior,
                } => write_manifest(
                    &mut self.manifest_output,
                    format_args!(
                        "tsobj {} {} d {} {} {} {} {} {} {}",
                        slot,
                        object.created_at,
                        ManifestName(schema.as_str()),
                        ManifestName(name.as_str()),
                        oid,
                        owner,
                        template,
                        ManifestName(options.as_str()),
                        text_search_behavior_code(behavior),
                    ),
                )?,
                crate::storage::TextSearchDefinition::Configuration {
                    schema,
                    name,
                    oid,
                    owner,
                    parser,
                    mappings,
                } => write_manifest(
                    &mut self.manifest_output,
                    format_args!(
                        "tsobj {} {} c {} {} {} {} {}{}",
                        slot,
                        object.created_at,
                        ManifestName(schema.as_str()),
                        ManifestName(name.as_str()),
                        oid,
                        owner,
                        parser,
                        ManifestTextSearchMappings(&mappings),
                    ),
                )?,
            }
        }
        for (slot, conversion) in storage.checkpoint_conversions() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                conversion.database,
            )?;
            let definition = conversion.definition;
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "conv {} {} {} {} {} {} {} {} {}",
                    slot,
                    conversion.created_at,
                    ManifestName(definition.schema.as_str()),
                    ManifestName(definition.name.as_str()),
                    definition.owner,
                    definition.source.code(),
                    definition.destination.code(),
                    definition.procedure,
                    u8::from(definition.default),
                ),
            )?;
        }
        for (slot, operator) in storage.checkpoint_operators() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                operator.database,
            )?;
            let definition = operator.definition;
            let result = definition
                .implementation
                .result()
                .unwrap_or(crate::storage::RoutineResult::TEXT);
            let function = definition.implementation.routine().unwrap_or(0);
            let left = definition
                .signature
                .left
                .unwrap_or(crate::storage::RoutineResult::TEXT);
            let right = definition
                .signature
                .right
                .unwrap_or(crate::storage::RoutineResult::TEXT);
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "opr {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
                    slot,
                    operator.created_at,
                    ManifestName(definition.schema.as_str()),
                    ManifestName(definition.name.as_str()),
                    definition.owner,
                    u8::from(definition.signature.left.is_some())
                        | (u8::from(definition.signature.right.is_some()) << 1),
                    ManifestRoutineResult(left),
                    ManifestRoutineResult(right),
                    ManifestRoutineResult(result),
                    function,
                    0,
                    0,
                    u8::from(definition.hashes),
                    u8::from(definition.merges),
                ),
            )?;
        }
        for (_, operator) in storage.checkpoint_operators() {
            let definition = operator.definition;
            if definition.commutator.is_none() && definition.negator.is_none() {
                continue;
            }
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                operator.database,
            )?;
            let linked_oid = |oid: Option<i32>| oid.unwrap_or(0);
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "oprl {} {} {}",
                    operator.oid(),
                    linked_oid(definition.commutator),
                    linked_oid(definition.negator),
                ),
            )?;
        }
        for (slot, family) in storage.checkpoint_operator_families() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                family.database,
            )?;
            let definition = family.definition;
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "opf {} {} {} {} {}",
                    slot,
                    family.created_at,
                    ManifestName(definition.schema.as_str()),
                    ManifestName(definition.name.as_str()),
                    definition.owner,
                ),
            )?;
            for member in definition.operators.iter().filter(|member| member.used) {
                write_manifest(
                    &mut self.manifest_output,
                    format_args!(
                        "opfo {} {} {} {} {}",
                        family.oid(),
                        member.strategy.number(),
                        ManifestRoutineResult(member.left),
                        ManifestRoutineResult(member.right),
                        member.operator,
                    ),
                )?;
            }
            for member in definition.functions.iter().filter(|member| member.used) {
                write_manifest(
                    &mut self.manifest_output,
                    format_args!(
                        "opff {} {} {} {}",
                        family.oid(),
                        ManifestRoutineResult(member.left),
                        ManifestRoutineResult(member.right),
                        member.function,
                    ),
                )?;
            }
        }
        for (slot, class) in storage.checkpoint_operator_classes() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                class.database,
            )?;
            let definition = class.definition;
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "opc {} {} {} {} {} {} {} {} {}",
                    slot,
                    class.created_at,
                    ManifestName(definition.schema.as_str()),
                    ManifestName(definition.name.as_str()),
                    definition.owner,
                    definition.family,
                    ManifestRoutineResult(definition.input),
                    ManifestRoutineResult(definition.storage),
                    u8::from(definition.default),
                ),
            )?;
            for member in definition.operators.iter().filter(|member| member.used) {
                write_manifest(
                    &mut self.manifest_output,
                    format_args!(
                        "opco {} {} {} {} {}",
                        class.oid(),
                        member.strategy.number(),
                        ManifestRoutineResult(member.left),
                        ManifestRoutineResult(member.right),
                        member.operator,
                    ),
                )?;
            }
            for member in definition.functions.iter().filter(|member| member.used) {
                write_manifest(
                    &mut self.manifest_output,
                    format_args!(
                        "opcf {} {} {} {}",
                        class.oid(),
                        ManifestRoutineResult(member.left),
                        ManifestRoutineResult(member.right),
                        member.function,
                    ),
                )?;
            }
        }
        for (slot, trigger) in storage.checkpoint_triggers() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                trigger.database,
            )?;
            use core::fmt::Write;
            let (target_kind, relation_schema, relation_name) = match trigger.target {
                crate::storage::TriggerTarget::Table(slot) => {
                    let table = storage.table_def(usize::from(slot), 0);
                    (0u8, table.schema, table.name)
                }
                crate::storage::TriggerTarget::View(slot) => {
                    let view = storage.view(usize::from(slot));
                    (1u8, view.schema, view.name)
                }
            };
            let function = storage.routine(usize::from(trigger.function));
            let mut hname = StackStr::<130>::new();
            let mut hschema = StackStr::<130>::new();
            let mut htable = StackStr::<130>::new();
            let mut hfunction_schema = StackStr::<130>::new();
            let mut hfunction = StackStr::<130>::new();
            let mut hold_table = StackStr::<130>::new();
            let mut hnew_table = StackStr::<130>::new();
            let mut hreferenced_schema = StackStr::<130>::new();
            let mut hreferenced_table = StackStr::<130>::new();
            let mut hwhen = StackStr::<{ crate::storage::TRIGGER_WHEN_MAX * 2 }>::new();
            let mut harguments = StackStr::<
                {
                    2 + crate::storage::MAX_TRIGGER_ARGUMENTS
                        * (2 + crate::storage::TRIGGER_ARGUMENT_BYTES * 2)
                },
            >::new();
            for byte in trigger.name.as_str().as_bytes() {
                let _ = write!(hname, "{byte:02x}");
            }
            for byte in relation_schema.as_str().as_bytes() {
                let _ = write!(hschema, "{byte:02x}");
            }
            for byte in relation_name.as_str().as_bytes() {
                let _ = write!(htable, "{byte:02x}");
            }
            for byte in function.schema.as_str().as_bytes() {
                let _ = write!(hfunction_schema, "{byte:02x}");
            }
            for byte in function.name.as_str().as_bytes() {
                let _ = write!(hfunction, "{byte:02x}");
            }
            if let Some(old) = trigger.transition_tables.old() {
                for byte in old.as_str().as_bytes() {
                    let _ = write!(hold_table, "{byte:02x}");
                }
            }
            if let Some(new) = trigger.transition_tables.new_table() {
                for byte in new.as_str().as_bytes() {
                    let _ = write!(hnew_table, "{byte:02x}");
                }
            }
            if let Some(when) = trigger.when {
                for byte in when.as_str().as_bytes() {
                    let _ = write!(hwhen, "{byte:02x}");
                }
            }
            let _ = write!(harguments, "{:02x}", trigger.arguments.values().len());
            for argument in trigger.arguments.values() {
                let _ = write!(harguments, "{:02x}", argument.as_str().len());
                for byte in argument.as_str().as_bytes() {
                    let _ = write!(harguments, "{byte:02x}");
                }
            }
            if let Some(referenced) = trigger.kind.referenced_table() {
                let definition = storage.table_def(usize::from(referenced), 0);
                for byte in definition.schema.as_str().as_bytes() {
                    let _ = write!(hreferenced_schema, "{byte:02x}");
                }
                for byte in definition.name.as_str().as_bytes() {
                    let _ = write!(hreferenced_table, "{byte:02x}");
                }
            }
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "trg {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
                    slot,
                    trigger.created_at,
                    target_kind,
                    hname.as_str(),
                    hschema.as_str(),
                    htable.as_str(),
                    hfunction_schema.as_str(),
                    hfunction.as_str(),
                    trigger.timing.code(),
                    trigger.level.code(),
                    trigger.events.bits(),
                    trigger.update_columns,
                    if trigger.transition_tables.old().is_some() {
                        hold_table.as_str()
                    } else {
                        "-"
                    },
                    if trigger.transition_tables.new_table().is_some() {
                        hnew_table.as_str()
                    } else {
                        "-"
                    },
                    if trigger.when.is_some() {
                        hwhen.as_str()
                    } else {
                        "-"
                    },
                    harguments.as_str(),
                    u8::from(matches!(
                        trigger.kind,
                        crate::storage::TriggerKind::Constraint { .. }
                    )),
                    trigger.kind.timing().code(),
                    if hreferenced_schema.as_str().is_empty() {
                        "-"
                    } else {
                        hreferenced_schema.as_str()
                    },
                    if hreferenced_table.as_str().is_empty() {
                        "-"
                    } else {
                        hreferenced_table.as_str()
                    },
                    trigger.enabled.code(),
                ),
            )?;
        }
        for (database, image) in storage.partition_trigger_states() {
            write_database_context(&mut self.manifest_output, &mut database_context, database)?;
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "trgs {} {} {} {} {} {}",
                    image.creation.get(),
                    image.trigger.slot(),
                    image.trigger.created_at(),
                    image.table.slot(),
                    image.table.created_at(),
                    image.enabled.code(),
                ),
            )?;
        }
        for (slot, policy) in storage.checkpoint_policies() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                policy.database,
            )?;
            use core::fmt::Write;
            let table = storage.table_def(usize::from(policy.table), 0);
            let definition = policy.definition_for(0);
            let mut schema = StackStr::<130>::new();
            let mut table_name = StackStr::<130>::new();
            let mut name = StackStr::<130>::new();
            let mut using = StackStr::<{ crate::storage::POLICY_EXPRESSION_MAX * 2 }>::new();
            let mut with_check = StackStr::<{ crate::storage::POLICY_EXPRESSION_MAX * 2 }>::new();
            for byte in table.schema.as_str().as_bytes() {
                let _ = write!(schema, "{byte:02x}");
            }
            for byte in table.name.as_str().as_bytes() {
                let _ = write!(table_name, "{byte:02x}");
            }
            for byte in policy.name.as_str().as_bytes() {
                let _ = write!(name, "{byte:02x}");
            }
            let roles = storage.policy_roles(slot, 0);
            if let Some(source) = definition.using {
                for byte in source.as_str().as_bytes() {
                    let _ = write!(using, "{byte:02x}");
                }
            }
            if let Some(source) = definition.with_check {
                for byte in source.as_str().as_bytes() {
                    let _ = write!(with_check, "{byte:02x}");
                }
            }
            // Serialize the role image before looking up dependencies. Both
            // images are mutex guarded, and dependency lookup consults the
            // policy definition to select its committed image.
            write!(
                &mut self.manifest_output,
                "pol {} {} {} {} {} {} {} {}{}",
                slot,
                policy.created_at,
                policy.command.code(),
                u8::from(policy.permissive),
                schema.as_str(),
                table_name.as_str(),
                name.as_str(),
                roles.len(),
                ManifestPolicyRoles {
                    storage,
                    roles: &roles,
                },
            )
            .map_err(|_| manifest_full())?;
            drop(roles);
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    " {} {} {}",
                    if definition.using.is_some() {
                        using.as_str()
                    } else {
                        "-"
                    },
                    if definition.with_check.is_some() {
                        with_check.as_str()
                    } else {
                        "-"
                    },
                    ManifestDependencies(storage.policy_dependencies(slot, 0).view()),
                ),
            )?;
        }
        for (_, extension) in storage.checkpoint_extensions() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                extension.database,
            )?;
            use core::fmt::Write;
            let mut name = StackStr::<130>::new();
            let mut schema = StackStr::<130>::new();
            let mut version = StackStr::<130>::new();
            for byte in extension.name.as_str().as_bytes() {
                let _ = write!(name, "{byte:02x}");
            }
            let schema_name = storage.schema_def(extension.namespace as usize).name;
            for byte in schema_name.as_str().as_bytes() {
                let _ = write!(schema, "{byte:02x}");
            }
            for byte in extension.version.as_str().as_bytes() {
                let _ = write!(version, "{byte:02x}");
            }
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "ext {} {} {} {} {} {}",
                    extension.created_at,
                    extension.ownership.owner,
                    u8::from(extension.relocatable),
                    name.as_str(),
                    schema.as_str(),
                    version.as_str(),
                ),
            )?;
        }
        // The availability catalog is durable independently of installed
        // extensions. SQL and control metadata are immutable objects; the CAS
        // manifest publishes their ordered, bounded catalog in one step.
        for (package_slot, package) in storage.extension_packages() {
            let metadata = encode_extension_package(*package)?;
            let metadata_key = stack_format!(
                80,
                "extensions/meta/{:08x}-{}.pkg",
                crate::wal::crc32c::crc32c(metadata.as_str().as_bytes()),
                metadata.as_str().len()
            );
            self.put_immutable(metadata_key.as_str(), metadata.as_str().as_bytes())?;
            write_manifest(
                &mut self.manifest_output,
                format_args!("xpk {} {}", package_slot, metadata_key.as_str()),
            )?;
            for (_, script) in storage.extension_scripts_for(package_slot) {
                let effective = encode_extension_package(script.effective)?;
                let effective_key = stack_format!(
                    80,
                    "extensions/meta/{:08x}-{}.pkg",
                    crate::wal::crc32c::crc32c(effective.as_str().as_bytes()),
                    effective.as_str().len()
                );
                self.put_immutable(effective_key.as_str(), effective.as_str().as_bytes())?;
                let source = storage.extension_script_source(*script).as_bytes();
                let source_key = stack_format!(
                    80,
                    "extensions/sql/{:08x}-{}.sql",
                    crate::wal::crc32c::crc32c(source),
                    source.len()
                );
                self.put_immutable(source_key.as_str(), source)?;
                let mut from = StackStr::<130>::new();
                let mut to = StackStr::<130>::new();
                use core::fmt::Write as _;
                if let Some(version) = script.from {
                    for byte in version.as_str().as_bytes() {
                        let _ = write!(from, "{byte:02x}");
                    }
                } else {
                    let _ = from.write_str("-");
                }
                for byte in script.to.as_str().as_bytes() {
                    let _ = write!(to, "{byte:02x}");
                }
                write_manifest(
                    &mut self.manifest_output,
                    format_args!(
                        "xsc {} {} {} {} {} {}",
                        package_slot,
                        from.as_str(),
                        to.as_str(),
                        effective_key.as_str(),
                        source_key.as_str(),
                        source.len()
                    ),
                )?;
            }
        }
        // Object comments: `cmt <class> <subid> <hex-schema> <hex-name>
        // <hex-text>`. Only committed comments carrying text are written.
        for comment in storage.checkpoint_comments() {
            if comment.class == crate::storage::CommentClass::Relation
                && comment.schema.as_str().starts_with("pg_temp_")
            {
                continue;
            }
            if let Some(database) = comment.database {
                write_database_context(&mut self.manifest_output, &mut database_context, database)?;
            }
            use core::fmt::Write;
            let Some(text) = comment.live else { continue };
            let mut hschema = StackStr::<130>::new();
            for b in comment.schema.as_str().as_bytes() {
                let _ = write!(hschema, "{b:02x}");
            }
            if hschema.as_str().is_empty() {
                let _ = write!(hschema, "-");
            }
            let mut hname = StackStr::<130>::new();
            for b in comment.name.as_str().as_bytes() {
                let _ = write!(hname, "{b:02x}");
            }
            let mut htext = StackStr::<{ crate::storage::COMMENT_MAX * 2 + 2 }>::new();
            for b in text.as_str().as_bytes() {
                let _ = write!(htext, "{b:02x}");
            }
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "cmt {} {} {} {} {}",
                    comment.class.to_u8(),
                    comment.subid,
                    hschema.as_str(),
                    hname.as_str(),
                    htext.as_str(),
                ),
            )?;
        }
        for (slot, tablespace) in storage.tablespaces_visible_to(0) {
            use core::fmt::Write;
            let mut name = StackStr::<130>::new();
            let mut location = StackStr::<{ crate::storage::TABLESPACE_LOCATION_MAX * 2 }>::new();
            for byte in tablespace.name.as_str().as_bytes() {
                let _ = write!(name, "{byte:02x}");
            }
            for byte in tablespace.location.as_str().as_bytes() {
                let _ = write!(location, "{byte:02x}");
            }
            if location.as_str().is_empty() {
                location = StackStr::from_str("-");
            }
            let options = tablespace.options;
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "tsp {} {} {} {} {} {} {} {} {}",
                    slot,
                    tablespace.created_at,
                    tablespace.ownership.owner,
                    name.as_str(),
                    location.as_str(),
                    options
                        .random_page_cost
                        .map_or(u64::MAX, crate::sql::ast::TablespaceCost::bits),
                    options
                        .seq_page_cost
                        .map_or(u64::MAX, crate::sql::ast::TablespaceCost::bits),
                    options.effective_io_concurrency.unwrap_or(i32::MIN),
                    options.maintenance_io_concurrency.unwrap_or(i32::MIN),
                ),
            )?;
        }
        // Index definitions are complete catalog state, not a cache rebuild hint.
        for (index_slot, index) in storage.checkpoint_indexes() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                index.database,
            )?;
            use core::fmt::Write;
            let mut columns = StackStr::<{ crate::storage::MAX_INDEX_COLS * 6 }>::new();
            for c in &index.columns[..index.n_cols] {
                let _ = write!(columns, "{c} ");
            }
            let mut includes = StackStr::<{ crate::storage::MAX_INDEX_COLS * 6 }>::new();
            for c in &index.include_columns[..index.n_include_cols] {
                let _ = write!(includes, "{c} ");
            }
            let mut hex_name = StackStr::<130>::new();
            for b in index.name.as_str().as_bytes() {
                let _ = write!(hex_name, "{b:02x}");
            }
            let mut htable = StackStr::<130>::new();
            for b in index.table.as_str().as_bytes() {
                let _ = write!(htable, "{b:02x}");
            }
            let mut hschema = StackStr::<130>::new();
            for b in index.schema.as_str().as_bytes() {
                let _ = write!(hschema, "{b:02x}");
            }
            let mut descending_mask = 0u32;
            let mut nulls_first_mask = 0u32;
            for i in 0..index.n_cols {
                descending_mask |= u32::from(index.descending[i]) << i;
                nulls_first_mask |= u32::from(index.nulls_first[i]) << i;
            }
            let predicate = match index.predicate {
                Some(text) => {
                    let mut hex =
                        StackStr::<{ crate::storage::INDEX_PREDICATE_MAX * 2 + 2 }>::new();
                    for byte in text.as_str().as_bytes() {
                        let _ = write!(hex, "{byte:02x}");
                    }
                    hex
                }
                None => StackStr::from_str("-"),
            };
            let mut expression_mask = 0u32;
            let mut encoded_expressions = StackStr::<
                { crate::storage::MAX_INDEX_COLS * (crate::storage::INDEX_EXPRESSION_MAX * 2 + 1) },
            >::new();
            for (position, expression) in index.expressions.iter().enumerate().take(index.n_cols) {
                let Some(expression) = expression else {
                    continue;
                };
                expression_mask |= 1u32 << position;
                let _ = encoded_expressions.write_str(" ");
                for byte in expression.as_str().as_bytes() {
                    let _ = write!(encoded_expressions, "{byte:02x}");
                }
            }
            let mut collations = StackStr::<{ crate::storage::MAX_INDEX_COLS * 8 }>::new();
            let mut operator_classes = StackStr::<{ crate::storage::MAX_INDEX_COLS * 14 }>::new();
            let mut resolved_operator_classes =
                StackStr::<{ crate::storage::MAX_INDEX_COLS * 14 }>::new();
            let mut operator_class_options =
                StackStr::<{ crate::storage::MAX_INDEX_COLS * 64 }>::new();
            let mut statistics = StackStr::<{ crate::storage::MAX_INDEX_COLS * 8 }>::new();
            let (maintenance, ranges) = storage.brin_maintenance_image(index_slot);
            let mut unsummarized_ranges =
                StackStr::<{ crate::storage::MAX_BRIN_UNSUMMARIZED_RANGES * 22 }>::new();
            for range in ranges.iter().take(usize::from(maintenance.count)) {
                let _ = write!(unsummarized_ranges, " {range}");
            }
            let mut maintenance_suffix =
                StackStr::<{ crate::storage::MAX_BRIN_UNSUMMARIZED_RANGES * 22 + 100 }>::new();
            if index.method == crate::sql::ast::IndexAccessMethod::Brin {
                let _ = write!(
                    maintenance_suffix,
                    " bu {} {} {}{}",
                    maintenance.pages_per_range,
                    maintenance.summarized_until_page,
                    maintenance.count,
                    unsummarized_ranges.as_str()
                );
            }
            for position in 0..index.n_cols {
                let _ = write!(
                    collations,
                    " {} {}",
                    index.collations[position].code(),
                    u8::from(index.explicit_collations[position])
                );
                match index.operator_classes[position] {
                    None => {
                        let _ = operator_classes.write_str(" 0");
                    }
                    Some(crate::storage::IndexOperatorClass::Btree(class)) => {
                        let _ = write!(operator_classes, " b{}", class.code());
                    }
                    Some(crate::storage::IndexOperatorClass::Hash(class)) => {
                        let _ = write!(operator_classes, " h{}", class.code());
                    }
                    Some(crate::storage::IndexOperatorClass::Brin(class)) => {
                        let _ = write!(operator_classes, " r{}", class.code());
                    }
                    Some(crate::storage::IndexOperatorClass::Gist(class)) => {
                        let _ = write!(operator_classes, " g{}", class.code());
                    }
                    Some(crate::storage::IndexOperatorClass::Gin(class)) => {
                        let _ = write!(operator_classes, " n{}", class.code());
                    }
                    Some(crate::storage::IndexOperatorClass::SpGist(class)) => {
                        let _ = write!(operator_classes, " s{}", class.code());
                    }
                    Some(crate::storage::IndexOperatorClass::Catalog(oid)) => {
                        let _ = write!(operator_classes, " c{}", oid.get());
                    }
                }
                match index.resolved_operator_classes[position]
                    .expect("live index key has a resolved operator class")
                {
                    crate::storage::IndexOperatorClass::Btree(class) => {
                        let _ = write!(resolved_operator_classes, " b{}", class.code());
                    }
                    crate::storage::IndexOperatorClass::Hash(class) => {
                        let _ = write!(resolved_operator_classes, " h{}", class.code());
                    }
                    crate::storage::IndexOperatorClass::Brin(class) => {
                        let _ = write!(resolved_operator_classes, " r{}", class.code());
                    }
                    crate::storage::IndexOperatorClass::Gist(class) => {
                        let _ = write!(resolved_operator_classes, " g{}", class.code());
                    }
                    crate::storage::IndexOperatorClass::Gin(class) => {
                        let _ = write!(resolved_operator_classes, " n{}", class.code());
                    }
                    crate::storage::IndexOperatorClass::SpGist(class) => {
                        let _ = write!(resolved_operator_classes, " s{}", class.code());
                    }
                    crate::storage::IndexOperatorClass::Catalog(oid) => {
                        let _ = write!(resolved_operator_classes, " c{}", oid.get());
                    }
                }
                let _ = write!(statistics, " {}", index.mutable.statistics[position]);
                let options = index.operator_class_options[position];
                let _ = write!(
                    operator_class_options,
                    " v{},n{},f{},s{}",
                    options
                        .values_per_range
                        .map_or_else(
                            || StackStr::<16>::from_str("-"),
                            |value| {
                                StackStr::<16>::from_str(stack_format!(16, "{value}").as_str())
                            }
                        )
                        .as_str(),
                    options
                        .n_distinct_per_range
                        .map_or_else(
                            || StackStr::<16>::from_str("-"),
                            |value| {
                                StackStr::<16>::from_str(stack_format!(16, "{value}").as_str())
                            }
                        )
                        .as_str(),
                    options
                        .false_positive_rate
                        .as_ref()
                        .map_or("-", |value| value.as_str()),
                    options
                        .siglen
                        .map_or_else(
                            || StackStr::<16>::from_str("-"),
                            |value| StackStr::<16>::from_str(stack_format!(16, "{value}").as_str()),
                        )
                        .as_str(),
                );
            }
            let mutable = index.mutable;
            let fillfactor = mutable.options.fillfactor.unwrap_or(0);
            let deduplicate = match mutable.options.deduplicate_items {
                None => 0,
                Some(false) => 1,
                Some(true) => 2,
            };
            let kind = match mutable.kind {
                crate::storage::IndexKind::Ordinary => 0,
                crate::storage::IndexKind::Partitioned { valid: false } => 1,
                crate::storage::IndexKind::Partitioned { valid: true } => 2,
            };
            let pages_per_range = mutable.options.pages_per_range.unwrap_or(0);
            let autosummarize = match mutable.options.autosummarize {
                None => 0,
                Some(false) => 1,
                Some(true) => 2,
            };
            let buffering = match mutable.options.buffering {
                None => 0,
                Some(crate::sql::ast::GistBuffering::Auto) => 1,
                Some(crate::sql::ast::GistBuffering::On) => 2,
                Some(crate::sql::ast::GistBuffering::Off) => 3,
            };
            let fastupdate = match mutable.options.fastupdate {
                None => 0,
                Some(false) => 1,
                Some(true) => 2,
            };
            let gin_pending_list_limit = mutable.options.gin_pending_list_limit.map_or_else(
                || StackStr::<16>::from_str("-"),
                |value| StackStr::<16>::from_str(stack_format!(16, "{value}").as_str()),
            );
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "idx {} {} {} {}{} {} {} {} {} {} {} {} {} {}{} {}{}{}{} {} {} {} {}{} {} {} {} {} {} {} gb{} bo{} gi{},{}{}",
                    index.created_at,
                    u8::from(index.unique),
                    index.n_cols,
                    columns.as_str(),
                    hex_name.as_str(),
                    htable.as_str(),
                    hschema.as_str(),
                    descending_mask,
                    nulls_first_mask,
                    predicate.as_str(),
                    index.n_include_cols,
                    includes.as_str(),
                    u8::from(index.nulls_not_distinct),
                    expression_mask,
                    encoded_expressions.as_str(),
                    index.n_cols,
                    collations.as_str(),
                    operator_classes.as_str(),
                    resolved_operator_classes.as_str(),
                    mutable.tablespace,
                    fillfactor,
                    deduplicate,
                    index.n_cols,
                    statistics.as_str(),
                    mutable.parent.unwrap_or(u16::MAX),
                    kind,
                    u8::from(mutable.clustered),
                    u8::from(mutable.replica_identity),
                    pages_per_range,
                    autosummarize,
                    buffering,
                    operator_class_options.as_str(),
                    fastupdate,
                    gin_pending_list_limit.as_str(),
                    maintenance_suffix.as_str(),
                ),
            )?;
        }
        for (_, dependency) in storage.checkpoint_extension_dependencies() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                storage
                    .access_object_database(dependency.object)
                    .expect("extension members are database-local"),
            )?;
            use core::fmt::Write;
            let extension = storage.extension(dependency.extension as usize).name;
            let (schema, name) = storage.access_object_name(dependency.object);
            let mut extension_hex = StackStr::<130>::new();
            let mut schema_hex = StackStr::<130>::new();
            let mut name_hex = StackStr::<130>::new();
            for byte in extension.as_str().as_bytes() {
                let _ = write!(extension_hex, "{byte:02x}");
            }
            for byte in schema.as_str().as_bytes() {
                let _ = write!(schema_hex, "{byte:02x}");
            }
            if schema_hex.as_str().is_empty() {
                let _ = write!(schema_hex, "-");
            }
            for byte in name.as_str().as_bytes() {
                let _ = write!(name_hex, "{byte:02x}");
            }
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "exd {} {} {} {} {} {}",
                    extension_hex.as_str(),
                    dependency.object.class as u8,
                    if dependency.object.class == crate::storage::AccessClass::Routine {
                        crate::storage::routine_oid(
                            &storage.routine_for(dependency.object.slot as usize, 0),
                        )
                    } else {
                        0
                    },
                    schema_hex.as_str(),
                    name_hex.as_str(),
                    match dependency.kind {
                        crate::storage::ExtensionDependencyKind::Member => 0,
                        crate::storage::ExtensionDependencyKind::Automatic => 1,
                        crate::storage::ExtensionDependencyKind::Required => 2,
                    },
                ),
            )?;
        }
        for (_, config) in storage.checkpoint_extension_configs() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                storage
                    .access_object_database(config.relation.access_object())
                    .expect("extension configuration relations are database-local"),
            )?;
            use core::fmt::Write;
            let extension = storage.extension(config.extension as usize).name;
            let (schema, name) = storage.access_object_name(config.relation.access_object());
            let mut extension_hex = StackStr::<130>::new();
            let mut schema_hex = StackStr::<130>::new();
            let mut name_hex = StackStr::<130>::new();
            let mut condition_hex =
                StackStr::<{ crate::storage::EXTENSION_CONFIG_CONDITION_BYTES * 2 }>::new();
            for byte in extension.as_str().as_bytes() {
                let _ = write!(extension_hex, "{byte:02x}");
            }
            for byte in schema.as_str().as_bytes() {
                let _ = write!(schema_hex, "{byte:02x}");
            }
            for byte in name.as_str().as_bytes() {
                let _ = write!(name_hex, "{byte:02x}");
            }
            for byte in config.condition.as_str().as_bytes() {
                let _ = write!(condition_hex, "{byte:02x}");
            }
            if condition_hex.as_str().is_empty() {
                let _ = write!(condition_hex, "-");
            }
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "exc {} {} {} {} {} {}",
                    extension_hex.as_str(),
                    config.ordinal,
                    config.relation.kind().to_u8(),
                    schema_hex.as_str(),
                    name_hex.as_str(),
                    condition_hex.as_str(),
                ),
            )?;
        }
        // Ownership and ACL authority follows every object definition so a
        // cold manifest load can resolve stable runtime slots from names.
        let mut write_owner = |object: crate::storage::AccessObject| -> Result<(), SqlError> {
            use core::fmt::Write;
            if storage.access_object_is_temporary(object, 0) {
                return Ok(());
            }
            if let Some(database) = storage.access_object_database(object) {
                write_database_context(&mut self.manifest_output, &mut database_context, database)?;
            }
            let (schema, name) = storage.access_object_name(object);
            let owner = storage.role(storage.object_owner(object, 0)).name;
            let mut schema_hex = StackStr::<130>::new();
            let mut name_hex = StackStr::<130>::new();
            let mut owner_hex = StackStr::<130>::new();
            for byte in schema.as_str().as_bytes() {
                let _ = write!(schema_hex, "{byte:02x}");
            }
            if schema_hex.as_str().is_empty() {
                let _ = write!(schema_hex, "-");
            }
            for byte in name.as_str().as_bytes() {
                let _ = write!(name_hex, "{byte:02x}");
            }
            for byte in owner.as_str().as_bytes() {
                let _ = write!(owner_hex, "{byte:02x}");
            }
            if object.class == crate::storage::AccessClass::Routine {
                write_manifest(
                    &mut self.manifest_output,
                    format_args!(
                        "own {} {} {} {} {}",
                        object.class as u8,
                        crate::storage::routine_oid(&storage.routine(object.slot as usize)),
                        schema_hex.as_str(),
                        name_hex.as_str(),
                        owner_hex.as_str()
                    ),
                )
            } else {
                write_manifest(
                    &mut self.manifest_output,
                    format_args!(
                        "own {} {} {} {}",
                        object.class as u8,
                        schema_hex.as_str(),
                        name_hex.as_str(),
                        owner_hex.as_str()
                    ),
                )
            }
        };
        for slot in 0..storage.table_count() {
            if storage.table(slot).live() {
                write_owner(crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Table,
                    slot: slot as u16,
                })?;
            }
        }
        for (slot, _) in storage.checkpoint_views() {
            write_owner(crate::storage::AccessObject {
                class: crate::storage::AccessClass::View,
                slot: slot as u16,
            })?;
        }
        for (slot, _) in storage.checkpoint_matviews() {
            write_owner(crate::storage::AccessObject {
                class: crate::storage::AccessClass::MaterializedView,
                slot: slot as u16,
            })?;
        }
        for (slot, _, _, _, _) in storage.checkpoint_sequences_with_slots() {
            write_owner(crate::storage::AccessObject {
                class: crate::storage::AccessClass::Sequence,
                slot: slot as u16,
            })?;
        }
        for (slot, schema) in storage.checkpoint_schemas() {
            if schema.has_database_owner(0) {
                continue;
            }
            write_owner(crate::storage::AccessObject {
                class: crate::storage::AccessClass::Schema,
                slot: slot as u16,
            })?;
        }
        for (slot, _) in storage.checkpoint_extensions() {
            write_owner(crate::storage::AccessObject {
                class: crate::storage::AccessClass::Extension,
                slot: slot as u16,
            })?;
        }
        for (slot, _) in storage.checkpoint_domains() {
            write_owner(crate::storage::AccessObject {
                class: crate::storage::AccessClass::Domain,
                slot: slot as u16,
            })?;
        }
        for (slot, _) in storage.checkpoint_enums() {
            write_owner(crate::storage::AccessObject {
                class: crate::storage::AccessClass::Enum,
                slot: slot as u16,
            })?;
        }
        for (slot, _) in storage.checkpoint_composites() {
            write_owner(crate::storage::AccessObject {
                class: crate::storage::AccessClass::Composite,
                slot: slot as u16,
            })?;
        }
        for (slot, _) in storage.checkpoint_indexes() {
            write_owner(crate::storage::AccessObject {
                class: crate::storage::AccessClass::Index,
                slot: slot as u16,
            })?;
        }
        for slot in 0..storage.routine_count() {
            if storage.routine(slot).visible_to(0) {
                write_owner(crate::storage::Storage::routine_access_object(slot))?;
            }
        }
        for (slot, _) in storage.checkpoint_large_objects() {
            write_owner(crate::storage::AccessObject {
                class: crate::storage::AccessClass::LargeObject,
                slot: slot as u16,
            })?;
        }
        for (_, acl) in storage.checkpoint_acls() {
            if storage.access_object_is_temporary(acl.object, 0) {
                continue;
            }
            if !storage.access_object_is_live_in_catalog(acl.object) {
                continue;
            }
            if let Some(database) = storage.access_object_database(acl.object) {
                write_database_context(&mut self.manifest_output, &mut database_context, database)?;
            }
            use core::fmt::Write;
            let (schema, name) = storage.access_object_name(acl.object);
            let grantee = if acl.grantee == crate::storage::PUBLIC_ROLE {
                crate::storage::SqlName::parse("PUBLIC").expect("PUBLIC fits")
            } else {
                storage.role(acl.grantee as usize).name
            };
            let grantor = storage.role(acl.grantor as usize).name;
            let mut schema_hex = StackStr::<130>::new();
            let mut name_hex = StackStr::<130>::new();
            let mut grantee_hex = StackStr::<130>::new();
            let mut grantor_hex = StackStr::<130>::new();
            for byte in schema.as_str().as_bytes() {
                let _ = write!(schema_hex, "{byte:02x}");
            }
            for byte in name.as_str().as_bytes() {
                let _ = write!(name_hex, "{byte:02x}");
            }
            for byte in grantee.as_str().as_bytes() {
                let _ = write!(grantee_hex, "{byte:02x}");
            }
            for byte in grantor.as_str().as_bytes() {
                let _ = write!(grantor_hex, "{byte:02x}");
            }
            if acl.object.class == crate::storage::AccessClass::Routine {
                write_manifest(
                    &mut self.manifest_output,
                    format_args!(
                        "acl {} {} {} {} {} {} {} {} {}",
                        acl.object.class as u8,
                        crate::storage::routine_oid(&storage.routine(acl.object.slot as usize)),
                        schema_hex.as_str(),
                        name_hex.as_str(),
                        grantee_hex.as_str(),
                        grantor_hex.as_str(),
                        acl.privileges.0,
                        acl.grant_options.0,
                        acl.order
                            .expect("checkpoint ACL has a committed grant order")
                            .get()
                    ),
                )?;
            } else {
                write_manifest(
                    &mut self.manifest_output,
                    format_args!(
                        "acl {} {} {} {} {} {} {} {}",
                        acl.object.class as u8,
                        schema_hex.as_str(),
                        name_hex.as_str(),
                        grantee_hex.as_str(),
                        grantor_hex.as_str(),
                        acl.privileges.0,
                        acl.grant_options.0,
                        acl.order
                            .expect("checkpoint ACL has a committed grant order")
                            .get()
                    ),
                )?;
            }
        }
        for (_, acl) in storage.checkpoint_column_acls() {
            if storage.access_object_is_temporary(acl.target.relation(), 0) {
                continue;
            }
            if !storage.access_object_is_live_in_catalog(acl.target.relation()) {
                continue;
            }
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                storage
                    .access_object_database(acl.target.relation())
                    .expect("column ACL relation is database-local"),
            )?;
            use core::fmt::Write;
            let relation = acl.target.relation();
            let (schema, name) = storage.access_object_name(relation);
            let grantee = if acl.grantee == crate::storage::PUBLIC_ROLE {
                crate::storage::SqlName::parse("PUBLIC").expect("PUBLIC fits")
            } else {
                storage.role(acl.grantee as usize).name
            };
            let grantor = storage.role(acl.grantor as usize).name;
            let mut schema_hex = StackStr::<130>::new();
            let mut name_hex = StackStr::<130>::new();
            let mut grantee_hex = StackStr::<130>::new();
            let mut grantor_hex = StackStr::<130>::new();
            for byte in schema.as_str().as_bytes() {
                let _ = write!(schema_hex, "{byte:02x}");
            }
            for byte in name.as_str().as_bytes() {
                let _ = write!(name_hex, "{byte:02x}");
            }
            for byte in grantee.as_str().as_bytes() {
                let _ = write!(grantee_hex, "{byte:02x}");
            }
            for byte in grantor.as_str().as_bytes() {
                let _ = write!(grantor_hex, "{byte:02x}");
            }
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "cacl {} {} {} {} {} {} {} {}",
                    relation.class as u8,
                    schema_hex.as_str(),
                    name_hex.as_str(),
                    acl.target.column(),
                    grantee_hex.as_str(),
                    grantor_hex.as_str(),
                    acl.privileges.0,
                    acl.grant_options.0
                ),
            )?;
        }
        for (slot, acl) in storage.checkpoint_default_acls() {
            write_database_context(
                &mut self.manifest_output,
                &mut database_context,
                acl.database,
            )?;
            use core::fmt::Write;
            let image = acl
                .committed
                .expect("checkpoint default privileges are present");
            let mut schema = StackStr::<32>::new();
            match image.identity.schema {
                Some(identity) => write!(&mut schema, "{}", identity.created_at()),
                None => write!(&mut schema, "-"),
            }
            .expect("default privilege namespace fits its exact decimal width");
            let mut grantee = StackStr::<32>::new();
            match image.grantee {
                Some(role) => write!(&mut grantee, "{}", role.get()),
                None => write!(&mut grantee, "PUBLIC"),
            }
            .expect("default privilege grantee fits its exact decimal width");
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "dacl {} {} {} {} {} {} {} {}",
                    slot,
                    image.identity.creation.created_at(),
                    image.identity.owner.get(),
                    schema.as_str(),
                    grantee.as_str(),
                    image.identity.class as u8,
                    image.privileges.0,
                    image.grant_options.0,
                ),
            )?;
        }
        for row in storage.parameter_acl_rows_visible(0) {
            self.parameter_acl_arena.reset();
            let image = storage
                .snapshot_parameter_acl_row(row.parameter, 0, &self.parameter_acl_arena)?
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "checkpoint parameter ACL row disappeared"
                    )
                })?;
            struct GrantsHex<'a>(crate::storage::ParameterAclGrants<'a>);
            impl core::fmt::Display for GrantsHex<'_> {
                fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                    if self.0.is_empty() {
                        return out.write_str("-");
                    }
                    for grant in self.0.iter() {
                        for byte in grant.encode() {
                            write!(out, "{byte:02x}")?;
                        }
                    }
                    Ok(())
                }
            }
            write_manifest(
                &mut self.manifest_output,
                format_args!(
                    "pacl {} {} {}",
                    image.oid().get(),
                    manifest_hex::<130>(row.parameter.as_str()),
                    GrantsHex(image.grants())
                ),
            )?;
        }
        write_manifest(&mut self.manifest_output, "end")?;
        self.captured_checkpoint_epoch = self.requested_checkpoint_epoch;

        Ok(())
    }

    pub(crate) fn request_checkpoint(&mut self) -> Result<CheckpointRequest, SqlError> {
        let epoch = self
            .requested_checkpoint_epoch
            .checked_add(1)
            .and_then(core::num::NonZeroU64::new)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "checkpoint request identities exhausted"
                )
            })?;
        self.requested_checkpoint_epoch = epoch.get();
        Ok(CheckpointRequest(epoch))
    }

    pub(crate) fn requested_checkpoint_epoch(&self) -> u64 {
        self.requested_checkpoint_epoch
    }

    pub(crate) fn checkpoint_request_satisfied(&self, request: CheckpointRequest) -> bool {
        self.published_checkpoint_epoch >= request.epoch()
    }

    pub(crate) fn manifest_installing(&self) -> bool {
        matches!(
            self.manifest_publication,
            Some(ManifestPublication::Installing { .. })
        )
    }

    pub(crate) fn manifest_publication_pending(&self) -> bool {
        self.manifest_publication.is_some()
    }

    pub(crate) fn manifest_publication_ready(&self, now: std::time::Instant) -> bool {
        match self.manifest_publication {
            Some(ManifestPublication::Uploading { .. }) => self.manifest_uploads.has_completion(),
            Some(ManifestPublication::Retry { at, .. }) => at <= now,
            Some(ManifestPublication::Installing { .. }) => true,
            None => false,
        }
    }

    fn queue_manifest_upload(&mut self, lsn: u64) -> Result<(), SqlError> {
        if self.writer_was_fenced() {
            return Err(sql_err!(
                SQLSTATE_CAS,
                "writer was fenced by a newer process"
            ));
        }
        self.manifest_publication = Some(ManifestPublication::Retry {
            lsn,
            at: std::time::Instant::now() + std::time::Duration::from_secs(1),
        });
        let condition = match self.manifest_etag {
            Some(etag) => crate::object_store::upload::Condition::PublishMatch(etag),
            None => crate::object_store::upload::Condition::PublishAbsent,
        };
        let ticket = self
            .manifest_uploads
            .enqueue(MANIFEST_KEY, self.manifest_output.readable(), condition)
            .map_err(upload_to_sql)?;
        self.manifest_publication = Some(ManifestPublication::Uploading { lsn, ticket });
        Ok(())
    }

    fn wait_manifest_publication(&mut self) -> Result<(), SqlError> {
        if self.manifest_publication_ready(std::time::Instant::now()) {
            return Ok(());
        }
        match self.manifest_publication {
            Some(ManifestPublication::Uploading { .. }) => self
                .manifest_uploads
                .wait_ready()
                .map_err(object_store_to_sql),
            Some(ManifestPublication::Retry { at, .. }) => self
                .manifest_uploads
                .wait_until(at)
                .map_err(object_store_to_sql),
            Some(ManifestPublication::Installing { .. }) | None => Ok(()),
        }
    }

    fn captured_table_current(&self, storage: &Storage, slot: usize) -> bool {
        slot < storage.physical_table_count()
            && storage.table(slot).live()
            && self.sliced_layout[slot] == SliceLayout::from_storage(storage, slot)
    }

    /// Once the conditional root is durable, each local installation advances
    /// exactly once. Errors retain the next uninstalled item, never the old CAS.
    pub(crate) fn advance_manifest_publication(
        &mut self,
        storage: &mut Storage,
    ) -> Result<CheckpointStep, SqlError> {
        let publication = self
            .manifest_publication
            .take()
            .expect("retained manifest publication");
        match publication {
            ManifestPublication::Uploading { lsn, ticket } => {
                self.manifest_uploads.advance(std::time::Instant::now());
                match self.manifest_uploads.take(ticket) {
                    Ok(Some(etag)) => {
                        self.manifest_etag = Some(etag);
                        self.manifest_present = true;
                        self.manifest_lsn = lsn;
                        std::mem::swap(&mut self.prev_ssts, &mut self.prev_scratch);
                        self.manifest_publication = Some(ManifestPublication::Installing {
                            lsn,
                            row_position: 0,
                            value_position: 0,
                            cleanup_position: 0,
                            cleanup_rows: None,
                        });
                    }
                    Ok(None) => {
                        self.manifest_publication =
                            Some(ManifestPublication::Uploading { lsn, ticket });
                    }
                    Err(crate::object_store::upload::UploadError::Request(error))
                        if error.is_precondition_failed() =>
                    {
                        self.writer_fenced = true;
                        return Err(sql_err!(
                            SQLSTATE_CAS,
                            "manifest root changed; writer was fenced"
                        ));
                    }
                    Err(error) => {
                        self.manifest_publication = Some(ManifestPublication::Retry {
                            lsn,
                            at: std::time::Instant::now() + std::time::Duration::from_secs(1),
                        });
                        return Err(upload_to_sql(error));
                    }
                }
            }
            ManifestPublication::Retry { lsn, at } => {
                if at > std::time::Instant::now() {
                    self.manifest_publication = Some(ManifestPublication::Retry { lsn, at });
                } else {
                    self.queue_manifest_upload(lsn)?;
                }
            }
            ManifestPublication::Installing {
                lsn,
                row_position,
                value_position,
                cleanup_position,
                mut cleanup_rows,
            } => {
                self.manifest_publication = Some(ManifestPublication::Installing {
                    lsn,
                    row_position,
                    value_position,
                    cleanup_position,
                    cleanup_rows,
                });
                if row_position < self.pending_installs.len() {
                    let (slot, install) = self.pending_installs[row_position].clone();
                    if self.captured_table_current(storage, slot) {
                        match install {
                            SlotInstall::Append {
                                reservation,
                                handle,
                                through_lsn,
                            } => storage.append_spill_through(&reservation, handle, through_lsn)?,
                            SlotInstall::Replace {
                                reservation,
                                handle,
                                through_lsn,
                            } => {
                                storage.collapse_spill_through(&reservation, handle, through_lsn)?
                            }
                            SlotInstall::Clear { target } => {
                                storage.clear_spill_publication(&target)?
                            }
                            SlotInstall::MergePair {
                                target,
                                expected,
                                at,
                                handle,
                            } => storage.merge_spill_pair(&target, at, expected, handle)?,
                        };
                    }
                    self.manifest_publication = Some(ManifestPublication::Installing {
                        lsn,
                        row_position: row_position + 1,
                        value_position,
                        cleanup_position,
                        cleanup_rows,
                    });
                } else if value_position < self.pending_value_installs.len() {
                    let install = self.pending_value_installs[value_position];
                    if self.captured_table_current(storage, install.slot)
                        && (0..storage.value_binding_count(install.slot)).any(|binding| {
                            Self::value_install_matches_binding(
                                storage,
                                &install,
                                install.slot,
                                binding,
                            )
                        })
                    {
                        storage.install_value_binding(
                            install.slot,
                            &install.columns[..install.n_columns],
                            install.index_created_at,
                            install.include_mask,
                            install.handle,
                            crate::storage::ValueIndexInstallation::Checkpoint {
                                dirty_lsn: install.dirty_lsn,
                            },
                        )?;
                    }
                    self.manifest_publication = Some(ManifestPublication::Installing {
                        lsn,
                        row_position,
                        value_position: value_position + 1,
                        cleanup_position,
                        cleanup_rows,
                    });
                } else if cleanup_position < self.checkpoint_deletions.len() {
                    let slot = cleanup_position;
                    if self.captured_table_current(storage, slot) {
                        if self.sliced_this_sweep[slot] {
                            let rows = cleanup_rows.get_or_insert_with(|| {
                                RowCleanup::new(
                                    RowCleanupTask::SavedMarkers {
                                        through_lsn: self.sliced_lsn[slot],
                                        generation: self.sliced_generation[slot],
                                    },
                                    RowCleanupTarget::Table(slot),
                                )
                            });
                            if storage.advance_row_cleanup(
                                rows,
                                core::num::NonZeroUsize::new(64).unwrap(),
                                MaintenanceMode::Background,
                            ) != MaintenanceStep::Complete
                            {
                                self.manifest_publication = Some(ManifestPublication::Installing {
                                    lsn,
                                    row_position,
                                    value_position,
                                    cleanup_position,
                                    cleanup_rows,
                                });
                                return Ok(CheckpointStep::Working);
                            }
                        }
                        storage.clear_table_dirty_at_generation(slot, self.sliced_generation[slot]);
                        if let Some(receipt) = self.checkpoint_statistics[slot].take() {
                            receipt.acknowledge(storage);
                        }
                    } else {
                        self.checkpoint_statistics[slot] = None;
                    }
                    self.checkpoint_deletions[slot] = None;
                    self.manifest_publication = Some(ManifestPublication::Installing {
                        lsn,
                        row_position,
                        value_position,
                        cleanup_position: slot + 1,
                        cleanup_rows: None,
                    });
                } else {
                    self.pending_installs.clear();
                    self.pending_value_installs.clear();
                    self.merge_done.fill(None);
                    self.sweeping = false;
                    self.published_checkpoint_epoch = self.captured_checkpoint_epoch;
                    self.manifest_publication = None;
                    self.published_lsn_pending_maintenance = Some(lsn);
                    self.legacy_garbage_pending = true;
                    self.block_garbage_pending = true;
                    return Ok(CheckpointStep::Published { lsn });
                }
            }
        }
        Ok(CheckpointStep::Working)
    }

    /// One beat's work for one table: computes its new SST list — carrying,
    /// delta-flushing, rebuilding when no durable base remains, and applying
    /// at most one completed paced merge —
    /// records it for the publish, and queues the storage installs that
    /// apply only after the manifest CAS lands. A compatible re-slice keeps
    /// the earlier immutable delta and appends only versions committed after
    /// its captured LSN. Relation replacements rebuild without a reusable
    /// published base; filled generation rosters are merged before this beat.
    fn begin_table_slice(&mut self, storage: &Storage, slot: usize) -> Result<bool, SqlError> {
        let layout = SliceLayout::from_storage(storage, slot);
        let reuse_slice = self.sliced_this_sweep[slot]
            && self.sliced_layout[slot] == layout
            && self.prev_scratch[slot].n < self.prev_scratch[slot].capacity();
        let slice_after_lsn = reuse_slice.then_some(self.sliced_lsn[slot]);
        if reuse_slice {
            self.slice_scratch.copy_from(&self.prev_scratch[slot]);
        } else {
            self.slice_scratch.copy_from(&self.prev_ssts[slot]);
        }
        let completed = (!reuse_slice).then(|| {
            self.merge_done[slot].as_ref().and_then(|done| {
                (pair_at(&self.slice_scratch, done.at)
                    == Some((done.old0.handle, done.old1.handle)))
                .then_some((done.at, done.merged, (done.old0.handle, done.old1.handle)))
            })
        });
        let mut installs = [const { None }; 2];
        let mut install_count = 0;
        if let Some((at, merged, expected)) = completed.flatten() {
            let target = storage.capture_spill_publication(
                slot,
                layout.created_at,
                layout.rewrite_generation,
            )?;
            self.slice_scratch.replace_pair(at, merged);
            installs[install_count] = Some(SlotInstall::MergePair {
                target,
                expected,
                at,
                handle: merged.map(|prior| prior.handle),
            });
            install_count += 1;
        }
        let clean = !storage.table(slot).dirty() && self.slice_scratch.n > 0;
        let delta = !clean
            && storage.table(slot).dirty()
            && self.slice_scratch.n > 0
            && self.slice_scratch.n < self.slice_scratch.capacity();
        let changed_after = delta.then_some(if reuse_slice {
            self.sliced_lsn[slot]
        } else {
            self.manifest_lsn
        });
        if storage.has_active_snapshots()
            && !clean
            && !storage.table(slot).spill_generations().is_empty()
            && !delta
        {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "historical snapshot pins a full SST generation list for table \"{}\"",
                storage.table(slot).definition().name.as_str()
            ));
        }
        let mut job = TableSliceJob {
            slot,
            layout,
            generation: storage.table(slot).generation(),
            through_lsn: storage.lsn(),
            reuse_slice,
            delta,
            count: 0,
            crc: Crc32c::new(),
            installs,
            install_count,
            phase: TableSlicePhase::Clean,
        };
        if clean {
            self.complete_table_slice(storage, job)?;
            return Ok(true);
        }
        #[cfg(feature = "checkpoint-profile")]
        let profile = (
            checkpoint_profile_start(),
            self.blocks
                .lock()
                .expect("block store lock poisoned")
                .io_stats(),
        );
        let deletion_guard = storage.retain_checkpoint_deletions(slot);
        let source = storage.table_row_snapshot(slot, None)?;
        if !source.matches_table(slot, layout.created_at) {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "table identity changed during checkpoint capture"
            ));
        }
        self.table_output.clear();
        let entries = &mut self.table_output;
        let mut collect = |rowid, state: crate::storage::RowRead<'_>| {
            use core::ops::ControlFlow;
            if changed_after.is_some_and(|floor| state.checkpoint_change_lsn <= floor) {
                return Ok(ControlFlow::Continue(()));
            }
            let mut version =
                |lsn: u64, home: Option<RowHome>, current: bool| -> Result<(), SqlError> {
                    if slice_after_lsn.is_some_and(|floor| lsn <= floor)
                        || (delta && !current && matches!(home, Some(RowHome::Spilled { .. })))
                    {
                        return Ok(());
                    }
                    entries
                        .push(CheckpointVersion {
                            key: SstKey::at(rowid, lsn),
                            home,
                        })
                        .map_err(|_| {
                            sql_err!(
                                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                "checkpoint_write_entries exhausted ({})",
                                entries.capacity()
                            )
                        })
                };
            if state.has_committed_version() {
                version(state.committed_lsn, state.committed, true)?;
            }
            for index in 0..state.history.len() {
                if let Some(prior) = state.history_get(index) {
                    version(prior.lsn, prior.home, false)?;
                }
            }
            Ok(ControlFlow::Continue(()))
        };
        if delta {
            let _ = storage.for_each_resident_row_state(slot, &mut collect)?;
        } else {
            storage.for_each_row_state(slot, &mut collect)?;
        }
        self.table_output
            .as_mut_slice()
            .sort_unstable_by_key(|entry| entry.key);
        self.slice_writer.reset();
        let mut schema = [ColType::Bool; MAX_COLUMNS];
        let columns = storage.table(slot).definition().schema(&mut schema);
        if delta {
            self.slice_writer.set_packed_rows().map_err(sst_to_sql)?;
        } else {
            self.slice_writer
                .set_pax_schema(&schema[..columns])
                .map_err(sst_to_sql)?;
        }
        self.slice_writer
            .set_packed_fillfactor(storage.table(slot).definition().storage_options.fillfactor)
            .map_err(sst_to_sql)?;
        job.phase = TableSlicePhase::Write {
            source,
            position: 0,
        };
        #[cfg(feature = "checkpoint-profile")]
        profile_checkpoint_phase(
            if job.delta {
                "row_sst_delta"
            } else {
                "row_sst_full"
            },
            job.through_lsn,
            Some(job.slot),
            profile.0,
            profile.1,
            self.blocks
                .lock()
                .expect("block store lock poisoned")
                .io_stats(),
            0,
        );
        self.checkpoint_deletions[slot].get_or_insert(deletion_guard);
        self.table_job = Some(job);
        Ok(false)
    }

    fn table_slice_matches(storage: &Storage, job: &TableSliceJob) -> bool {
        job.slot < storage.physical_table_count()
            && storage.table(job.slot).live()
            && SliceLayout::from_storage(storage, job.slot) == job.layout
    }

    fn advance_table_slice(&mut self, storage: &Storage) -> Result<(), SqlError> {
        let mut job = self.table_job.take().expect("scheduled table output");
        if !Self::table_slice_matches(storage, &job) {
            self.block_output
                .discard(&mut self.block_uploads)
                .map_err(upload_to_sql)?;
            self.table_output.clear();
            self.slice_writer.reset();
            return Ok(());
        }
        if matches!(
            job.phase,
            TableSlicePhase::Finish(_) | TableSlicePhase::Clean
        ) {
            return self.complete_table_slice(storage, job);
        }
        #[cfg(feature = "checkpoint-profile")]
        let profile = (
            if job.delta {
                "row_sst_delta"
            } else {
                "row_sst_full"
            },
            job.through_lsn,
            job.slot,
            checkpoint_profile_start(),
            self.blocks
                .lock()
                .expect("block store lock poisoned")
                .io_stats(),
        );
        let mut writer_failed = false;
        let result = (|| -> Result<(), SqlError> {
            let TableSlicePhase::Write { source, position } = &mut job.phase else {
                unreachable!("checked table output phase");
            };
            let mut processed = 0;
            while *position < self.table_output.len()
                && processed < 64
                && self.block_output.is_empty()
            {
                let entry = self.table_output[*position];
                let mut header = [0u8; VERSIONED_SST_ENTRY_HEADER];
                header[..8].copy_from_slice(&entry.key.rowid.to_le_bytes());
                header[8..16].copy_from_slice(&entry.key.commit_lsn.to_le_bytes());
                header[16..20].copy_from_slice(
                    &entry
                        .home
                        .map_or(u32::MAX, |home| match home {
                            RowHome::Heap(location) => location.len,
                            RowHome::Spilled { len, .. } => len,
                        })
                        .to_le_bytes(),
                );
                let appended = if let Some(home) = entry.home {
                    source.with_physical_row_bytes(entry.key.rowid, home, |row| {
                        let appended = self.slice_writer.append_version(
                            &mut self.block_output,
                            entry.key,
                            row,
                        );
                        if appended.is_ok() {
                            job.crc.update(&header);
                            job.crc.update(row);
                        }
                        Ok(appended)
                    })?
                } else {
                    let appended = self
                        .slice_writer
                        .append_tombstone_version(&mut self.block_output, entry.key);
                    if appended.is_ok() {
                        job.crc.update(&header);
                    }
                    appended
                };
                if let Err(error) = appended {
                    writer_failed = true;
                    return Err(sst_to_sql(error));
                }
                job.count += 1;
                *position += 1;
                processed += 1;
            }
            if *position == self.table_output.len() && self.block_output.is_empty() {
                match self.slice_writer.finish(&mut self.block_output) {
                    Ok(handle) => job.phase = TableSlicePhase::Finish(handle),
                    Err(error) => {
                        writer_failed = true;
                        return Err(sst_to_sql(error));
                    }
                }
            }
            Ok(())
        })();
        #[cfg(feature = "checkpoint-profile")]
        profile_checkpoint_phase(
            profile.0,
            profile.1,
            Some(profile.2),
            profile.3,
            profile.4,
            self.blocks
                .lock()
                .expect("block store lock poisoned")
                .io_stats(),
            0,
        );
        if writer_failed {
            self.block_output
                .discard(&mut self.block_uploads)
                .map_err(upload_to_sql)?;
            self.table_output.clear();
            self.slice_writer.reset();
        } else {
            self.table_job = Some(job);
        }
        result
    }

    fn complete_table_slice(
        &mut self,
        storage: &Storage,
        mut job: TableSliceJob,
    ) -> Result<(), SqlError> {
        if !Self::table_slice_matches(storage, &job) {
            self.table_output.clear();
            return Ok(());
        }
        match job.phase {
            TableSlicePhase::Finish(handle) => {
                let crc = job.crc.finish();
                match (job.delta, handle) {
                    (true, Some(handle)) => {
                        let reservation = storage.reserve_spill_generation(
                            job.slot,
                            job.layout.created_at,
                            job.layout.rewrite_generation,
                        )?;
                        push_slot_list(
                            &mut self.slice_scratch,
                            PrevSst {
                                handle,
                                count: job.count,
                                crc,
                            },
                        )?;
                        job.installs[job.install_count] = Some(SlotInstall::Append {
                            reservation,
                            handle,
                            through_lsn: job.through_lsn,
                        });
                        job.install_count += 1;
                    }
                    (true, None) => {}
                    (false, handle) => {
                        job.installs[job.install_count] = Some(match handle {
                            Some(handle) => SlotInstall::Replace {
                                reservation: storage.reserve_spill_generation(
                                    job.slot,
                                    job.layout.created_at,
                                    job.layout.rewrite_generation,
                                )?,
                                handle,
                                through_lsn: job.through_lsn,
                            },
                            None => SlotInstall::Clear {
                                target: storage.capture_spill_publication(
                                    job.slot,
                                    job.layout.created_at,
                                    job.layout.rewrite_generation,
                                )?,
                            },
                        });
                        job.install_count += 1;
                        self.slice_scratch.clear();
                        if let Some(handle) = handle {
                            push_slot_list(
                                &mut self.slice_scratch,
                                PrevSst {
                                    handle,
                                    count: job.count,
                                    crc,
                                },
                            )?;
                        }
                    }
                }
            }
            TableSlicePhase::Clean => {}
            TableSlicePhase::Write { .. } => {
                unreachable!("table output must finish before installation")
            }
        }
        if !job.reuse_slice {
            self.pending_installs.retain(|(slot, _)| *slot != job.slot);
        }
        for install in job.installs[..job.install_count].iter().flatten().cloned() {
            self.pending_installs.push((job.slot, install));
        }
        self.prev_scratch[job.slot].copy_from(&self.slice_scratch);
        self.sliced_generation[job.slot] = job.generation;
        self.sliced_lsn[job.slot] = job.through_lsn;
        self.sliced_layout[job.slot] = job.layout;
        self.sliced_this_sweep[job.slot] = true;
        self.table_output.clear();
        Ok(())
    }

    fn value_install_matches_binding(
        storage: &Storage,
        install: &ValueInstall,
        slot: usize,
        binding: usize,
    ) -> bool {
        if install.slot != slot
            || slot >= storage.physical_table_count()
            || !storage.table(slot).live()
            || storage.table(slot).created_at() != install.table_created_at
            || binding >= storage.value_binding_count(slot)
            || !storage.value_binding_is_committed(slot, binding)
        {
            return false;
        }
        let (columns, n_columns) = storage.value_binding_columns(slot, binding);
        install.n_columns == n_columns
            && install.columns[..n_columns] == columns[..n_columns]
            && install.index_created_at == storage.value_binding_created_at(slot, binding)
            && install.include_mask == storage.value_binding_include_mask(slot, binding)
    }

    fn value_install_is_current(storage: &Storage, install: &ValueInstall) -> bool {
        if install.slot >= storage.physical_table_count() {
            return false;
        }
        (0..storage.value_binding_count(install.slot)).any(|binding| {
            Self::value_install_matches_binding(storage, install, install.slot, binding)
                && storage.value_binding_dirty_lsn(install.slot, binding) == Some(install.dirty_lsn)
        })
    }

    fn value_index_schedule_pending(&self, storage: &Storage) -> bool {
        (0..storage.physical_table_count()).any(|slot| {
            let table = storage.table(slot);
            table.live()
                && {
                    let definition = table.definition();
                    definition.persistence
                } != crate::storage::RelationPersistence::Temporary
                && (0..storage.value_binding_count(slot)).any(|binding| {
                    storage.value_binding_is_committed(slot, binding)
                        && storage.value_binding_needs_publish(slot, binding)
                        && !self.pending_value_installs.iter().any(|install| {
                            Self::value_install_matches_binding(storage, install, slot, binding)
                                && storage.value_binding_dirty_lsn(slot, binding)
                                    == Some(install.dirty_lsn)
                        })
                })
        })
    }

    /// Starts one affected binding. Entry collection and external sorting are
    /// one schedule beat; the immutable writer consumes the retained result
    /// through bounded later beats.
    fn start_value_index_job(&mut self, storage: &Storage, slot: usize) -> Result<bool, SqlError> {
        self.pending_value_installs
            .retain(|install| Self::value_install_is_current(storage, install));
        let binding = (0..storage.value_binding_count(slot)).find(|&binding| {
            storage.value_binding_is_committed(slot, binding)
                && storage.value_binding_needs_publish(slot, binding)
                && !self.pending_value_installs.iter().any(|install| {
                    Self::value_install_matches_binding(storage, install, slot, binding)
                })
        });
        let Some(binding) = binding else {
            return Ok(false);
        };
        if self.pending_value_installs.len() == self.pending_value_installs.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "checkpoint value-index install roster is full"
            ));
        }

        #[cfg(feature = "checkpoint-profile")]
        let started = checkpoint_profile_start();
        #[cfg(feature = "checkpoint-profile")]
        let before = self
            .blocks
            .lock()
            .expect("block store lock poisoned")
            .io_stats();
        let through_lsn = storage.lsn();
        let dirty_lsn = storage
            .value_binding_dirty_lsn(slot, binding)
            .expect("selected value binding is dirty");
        let base = storage
            .value_binding_handle(slot, binding)
            .filter(|_| !storage.value_binding_requires_rebuild(slot, binding));
        self.sst_arena.reset();
        let roster_read = self
            .sst_arena
            .alloc_slice_with(crate::store::MAX_PAYLOAD, |_| 0u8)
            .map_err(|_| sql_err!(SQLSTATE_IO, "persistent value-index roster scratch"))?;
        self.roster_scratch.clear();
        self.value_changed_rowids.clear();
        let result = (|| {
            if let Some(previous) = base {
                let known = &mut self.roster_scratch;
                let complete = crate::store::walk_value_roster(
                    &mut *self.blocks.lock().expect("block store lock poisoned"),
                    previous.roster,
                    roster_read,
                    |id, kind| {
                        if known.len() == known.capacity() {
                            return false;
                        }
                        known.push((id, Some(kind)));
                        true
                    },
                )
                .map_err(value_index_to_sql)?;
                if !complete {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "published value-index blocks exceed checkpoint_live_blocks ({})",
                        self.roster_scratch.capacity()
                    ));
                }
                self.roster_scratch.sort_unstable();
                self.roster_scratch.dedup();
            }
            self.value_source_cursor.reset();
            self.slice_writer.reset();
            let (columns, n_columns) = storage.value_binding_columns(slot, binding);
            self.value_schedule_job = Some(ValueScheduleJob {
                slot,
                table_created_at: storage.table(slot).created_at(),
                table_generation: storage.table(slot).generation(),
                binding,
                columns,
                n_columns,
                index_created_at: storage.value_binding_created_at(slot, binding),
                include_mask: storage.value_binding_include_mask(slot, binding),
                navigation: storage.value_binding_navigation(slot, binding),
                dirty_lsn,
                through_lsn,
                base,
                phase: ValueSchedulePhase::Collect,
                source_done: false,
                pending: None,
                chunk_len: 0,
                row_count: 0,
                next_ordinal: 0,
                levels: [None; 64],
            });
            Ok(())
        })();
        #[cfg(feature = "checkpoint-profile")]
        profile_checkpoint_phase(
            "value_index_schedule",
            through_lsn,
            Some(slot),
            started,
            before,
            self.blocks
                .lock()
                .expect("block store lock poisoned")
                .io_stats(),
            0,
        );
        if result.is_err() {
            self.roster_scratch.clear();
            self.slice_writer.reset();
        }
        result?;
        Ok(true)
    }

    fn value_schedule_job_is_current(storage: &Storage, job: &ValueScheduleJob) -> bool {
        if job.slot >= storage.physical_table_count()
            || !storage.table(job.slot).live()
            || storage.table(job.slot).created_at() != job.table_created_at
            || storage.table(job.slot).generation() != job.table_generation
            || job.binding >= storage.value_binding_count(job.slot)
            || !storage.value_binding_is_committed(job.slot, job.binding)
        {
            return false;
        }
        let (columns, n_columns) = storage.value_binding_columns(job.slot, job.binding);
        job.n_columns == n_columns
            && job.columns[..n_columns] == columns[..n_columns]
            && job.index_created_at == storage.value_binding_created_at(job.slot, job.binding)
            && job.include_mask == storage.value_binding_include_mask(job.slot, job.binding)
            && job.navigation == storage.value_binding_navigation(job.slot, job.binding)
            && storage.value_binding_dirty_lsn(job.slot, job.binding) == Some(job.dirty_lsn)
    }

    fn compare_value_sort_entries(
        storage: &Storage,
        job: &ValueScheduleJob,
        left: &[u8],
        right: &[u8],
    ) -> Result<Ordering, SqlError> {
        Self::compare_value_entries(storage, job.slot, job.binding, job.navigation, left, right)
    }

    fn compare_value_entries(
        storage: &Storage,
        slot: usize,
        binding: usize,
        navigation: Option<crate::store::NavigationSpec>,
        left: &[u8],
        right: &[u8],
    ) -> Result<Ordering, SqlError> {
        if navigation
            .is_some_and(|navigation| navigation.kind == crate::store::NavigationKind::Posting)
        {
            compare_posting_sort_entries(left, right)
        } else {
            storage.compare_value_binding_keys(
                0,
                slot,
                binding,
                navigation,
                value_sort_key(left)?,
                value_sort_key(right)?,
            )
        }
    }

    fn push_value_sort_entry(
        value_source: &mut [u8],
        value_sort_rows: &mut [BufferedValueRow],
        job: &mut ValueScheduleJob,
        entry: &[u8],
    ) -> Result<bool, SqlError> {
        let total = 8usize.checked_add(entry.len()).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "value-index sort entry length overflows"
            )
        })?;
        if total > MAX_INLINE_ROW {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "one value-index sort entry exceeds the external-run block capacity"
            ));
        }
        if job.row_count == value_sort_rows.len() || job.chunk_len + total > value_source.len() {
            return Ok(false);
        }
        let offset = job.chunk_len;
        value_source[offset..offset + 8].copy_from_slice(&job.next_ordinal.to_le_bytes());
        value_source[offset + 8..offset + total].copy_from_slice(entry);
        value_sort_rows[job.row_count] = BufferedValueRow {
            offset: offset as u32,
            length: total as u32,
            ordinal: job.next_ordinal,
        };
        job.row_count += 1;
        job.chunk_len += total;
        job.next_ordinal = job.next_ordinal.checked_add(1).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "external query row ordinal exhausted"
            )
        })?;
        Ok(true)
    }

    fn sort_value_chunk(
        &mut self,
        storage: &Storage,
        job: &ValueScheduleJob,
    ) -> Result<(), SqlError> {
        let source = &self.value_source;
        let mut comparison_error = None;
        self.value_sort_rows[..job.row_count].sort_unstable_by(|left, right| {
            if comparison_error.is_some() {
                return Ordering::Equal;
            }
            let row = |entry: &BufferedValueRow| {
                let start = entry.offset as usize + 8;
                let end = entry.offset as usize + entry.length as usize;
                &source[start..end]
            };
            match Self::compare_value_sort_entries(storage, job, row(left), row(right)) {
                Ok(order) => order.then_with(|| left.ordinal.cmp(&right.ordinal)),
                Err(error) => {
                    comparison_error = Some(error);
                    Ordering::Equal
                }
            }
        });
        if let Some(error) = comparison_error {
            return Err(error);
        }
        Ok(())
    }

    #[expect(clippy::too_many_arguments, reason = "fixed sort buffers and row view")]
    fn encode_value_sort_row(
        storage: &Storage,
        value_source: &mut [u8],
        value_sort_rows: &mut [BufferedValueRow],
        job: &mut ValueScheduleJob,
        row: PendingValueRow,
        key: &[u8],
        payload: &[u8],
        sorted_entry: &mut [u8],
    ) -> Result<bool, SqlError> {
        if job
            .navigation
            .is_some_and(|navigation| navigation.kind == crate::store::NavigationKind::Posting)
        {
            let mut token_index = 0usize;
            let mut complete = true;
            let mut token_error = None;
            storage.for_each_value_binding_posting_token(job.slot, job.binding, key, |token| {
                if token_error.is_some() || !complete {
                    return;
                }
                if token_index < row.token_skip {
                    token_index += 1;
                    return;
                }
                let posting_key = token.encode();
                let entry_len = VALUE_SORT_ENTRY_HEADER + posting_key.len();
                sorted_entry[..8].copy_from_slice(&token.hash().to_le_bytes());
                sorted_entry[8..16].copy_from_slice(&row.rowid.to_le_bytes());
                sorted_entry[16..24].copy_from_slice(&row.commit_lsn.to_le_bytes());
                sorted_entry[24..28].copy_from_slice(&(posting_key.len() as u32).to_le_bytes());
                sorted_entry[28..32].fill(0);
                sorted_entry[VALUE_SORT_ENTRY_HEADER..entry_len].copy_from_slice(&posting_key);
                match Self::push_value_sort_entry(
                    value_source,
                    value_sort_rows,
                    job,
                    &sorted_entry[..entry_len],
                ) {
                    Ok(true) => token_index += 1,
                    Ok(false) => {
                        job.pending = Some(PendingValueRow {
                            token_skip: token_index,
                            ..row
                        });
                        complete = false;
                    }
                    Err(error) => token_error = Some(error),
                }
            })?;
            if let Some(error) = token_error {
                return Err(error);
            }
            return Ok(complete);
        }
        let entry_len = VALUE_SORT_ENTRY_HEADER + key.len() + payload.len();
        sorted_entry[..8].copy_from_slice(&row.hash.to_le_bytes());
        sorted_entry[8..16].copy_from_slice(&row.rowid.to_le_bytes());
        sorted_entry[16..24].copy_from_slice(&row.commit_lsn.to_le_bytes());
        sorted_entry[24..28].copy_from_slice(&(key.len() as u32).to_le_bytes());
        sorted_entry[28..32].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        sorted_entry[VALUE_SORT_ENTRY_HEADER..VALUE_SORT_ENTRY_HEADER + key.len()]
            .copy_from_slice(key);
        sorted_entry[VALUE_SORT_ENTRY_HEADER + key.len()..entry_len].copy_from_slice(payload);
        if Self::push_value_sort_entry(
            value_source,
            value_sort_rows,
            job,
            &sorted_entry[..entry_len],
        )? {
            Ok(true)
        } else {
            job.pending = Some(row);
            Ok(false)
        }
    }

    fn value_collect_beat(
        &mut self,
        storage: &Storage,
        job: &mut ValueScheduleJob,
    ) -> Result<Option<ValueIndexSource>, SqlError> {
        self.sst_arena.reset();
        let key_output = self
            .sst_arena
            .alloc_slice_with(crate::store::MAX_PAYLOAD, |_| 0u8)
            .map_err(|_| sql_err!(SQLSTATE_IO, "persistent value-index key scratch"))?;
        let sorted_entry = self
            .sst_arena
            .alloc_slice_with(crate::store::MAX_PAYLOAD, |_| 0u8)
            .map_err(|_| sql_err!(SQLSTATE_IO, "persistent value-index sort scratch"))?;

        if let Some(pending) = job.pending.take() {
            let pending_len = pending.key_len + pending.payload_len;
            self.value_entry[..pending_len].copy_from_slice(&self.value_source[..pending_len]);
            let (key, payload) = self.value_entry[..pending_len].split_at(pending.key_len);
            if !Self::encode_value_sort_row(
                storage,
                &mut self.value_source,
                &mut self.value_sort_rows,
                job,
                pending,
                key,
                payload,
                sorted_entry,
            )? {
                self.sort_value_chunk(storage, job)?;
                job.phase = ValueSchedulePhase::WriteChunk { position: 0 };
                return Ok(None);
            }
        }

        if !job.source_done {
            let mut stopped = false;
            let slot = job.slot;
            let binding = job.binding;
            let base = job.base;
            let mut each = |entry: crate::storage::CheckpointValueEntry<'_>| {
                let pending = PendingValueRow {
                    rowid: entry.rowid,
                    commit_lsn: entry.commit_lsn,
                    hash: entry.hash,
                    key_len: entry.key.len(),
                    payload_len: entry.payload.len(),
                    token_skip: 0,
                };
                if Self::encode_value_sort_row(
                    storage,
                    &mut self.value_source,
                    &mut self.value_sort_rows,
                    job,
                    pending,
                    entry.key,
                    entry.payload,
                    sorted_entry,
                )? {
                    return Ok(core::ops::ControlFlow::Continue(()));
                }
                self.value_entry[..entry.key.len()].copy_from_slice(entry.key);
                self.value_entry[entry.key.len()..entry.key.len() + entry.payload.len()]
                    .copy_from_slice(entry.payload);
                stopped = true;
                Ok(core::ops::ControlFlow::Break(()))
            };
            let done = if let Some(base) = base {
                storage.for_each_value_binding_delta_entry_batch(
                    slot,
                    binding,
                    base.published_lsn,
                    &mut self.value_source_cursor,
                    &mut self.value_changed_rowids,
                    key_output,
                    VALUE_INDEX_SCHEDULE_BEAT_ROWS,
                    &mut each,
                )?
            } else {
                storage.for_each_value_binding_entry_batch(
                    slot,
                    binding,
                    &mut self.value_source_cursor,
                    &self.sst_arena,
                    key_output,
                    VALUE_INDEX_SCHEDULE_BEAT_ROWS,
                    VALUE_INDEX_SCHEDULE_BEAT_READS,
                    &mut each,
                )?
            };
            job.source_done = done && !stopped;
        }

        if job.pending.is_some()
            || (!job.source_done && job.row_count == self.value_sort_rows.len())
        {
            self.sort_value_chunk(storage, job)?;
            job.phase = ValueSchedulePhase::WriteChunk { position: 0 };
            return Ok(None);
        }
        if !job.source_done {
            return Ok(None);
        }
        if job.levels.iter().all(Option::is_none) {
            self.sort_value_chunk(storage, job)?;
            return Ok(Some(ValueIndexSource::InMemory {
                rows: job.row_count,
                position: 0,
            }));
        }
        if job.row_count != 0 {
            self.sort_value_chunk(storage, job)?;
            job.phase = ValueSchedulePhase::WriteChunk { position: 0 };
        } else {
            job.phase = ValueSchedulePhase::Finish {
                accumulator: None,
                level: 0,
            };
        }
        Ok(None)
    }

    fn install_value_run(
        job: &mut ValueScheduleJob,
        run: ExternalRun,
        destination: ValueMergeDestination,
    ) -> Result<(), SqlError> {
        match destination {
            ValueMergeDestination::Carry(level) => {
                if level == job.levels.len() {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "checkpoint value-index external run levels exhausted"
                    ));
                }
                if let Some(left) = job.levels[level].take() {
                    job.phase = ValueSchedulePhase::StartMerge {
                        left,
                        right: run,
                        destination: ValueMergeDestination::Carry(level + 1),
                    };
                } else {
                    job.levels[level] = Some(run);
                    job.phase = if job.source_done && job.pending.is_none() {
                        ValueSchedulePhase::Finish {
                            accumulator: None,
                            level: 0,
                        }
                    } else {
                        ValueSchedulePhase::Collect
                    };
                }
            }
            ValueMergeDestination::Finish(level) => {
                job.phase = ValueSchedulePhase::Finish {
                    accumulator: Some(run),
                    level,
                };
            }
        }
        Ok(())
    }

    fn value_write_chunk_beat(
        &mut self,
        job: &mut ValueScheduleJob,
        mut position: usize,
    ) -> Result<(), SqlError> {
        if position == 0 {
            self.slice_writer.reset();
        }
        let start = position;
        while position < job.row_count {
            let row = self.value_sort_rows[position];
            let begin = row.offset as usize;
            let end = begin + row.length as usize;
            self.slice_writer
                .append(
                    &mut self.block_output,
                    position as u64 + 1,
                    &self.value_source[begin..end],
                )
                .map_err(sst_to_sql)?;
            position += 1;
            if position - start >= VALUE_INDEX_WRITE_BEAT_ENTRIES || !self.block_output.is_empty() {
                job.phase = ValueSchedulePhase::WriteChunk { position };
                return Ok(());
            }
        }
        if !self.block_output.is_empty() {
            job.phase = ValueSchedulePhase::WriteChunk { position };
            return Ok(());
        }
        let rows = job.row_count as u64;
        let handle = self
            .slice_writer
            .finish(&mut self.block_output)
            .map_err(sst_to_sql)?
            .expect("non-empty value-index run");
        if let Some(pending) = job.pending {
            let pending_len = pending.key_len + pending.payload_len;
            // A carry merge reuses `value_entry`; retain the deferred source
            // row in the now-idle chunk buffer until collection resumes.
            self.value_source[..pending_len].copy_from_slice(&self.value_entry[..pending_len]);
        }
        job.chunk_len = 0;
        job.row_count = 0;
        job.phase = ValueSchedulePhase::CompleteRun {
            run: ExternalRun::from_checkpoint(handle, rows),
            destination: ValueMergeDestination::Carry(0),
        };
        Ok(())
    }

    fn start_value_merge(
        &mut self,
        job: &mut ValueScheduleJob,
        left: ExternalRun,
        right: ExternalRun,
        destination: ValueMergeDestination,
    ) -> Result<(), SqlError> {
        self.value_sort_reader.start(
            &mut *self.blocks.lock().expect("block store lock poisoned"),
            left,
        )?;
        self.sst_arena.reset();
        let index = self
            .sst_arena
            .alloc_slice_with(crate::store::MAX_PAYLOAD, |_| 0u8)
            .map_err(|_| sql_err!(SQLSTATE_IO, "value-index merge index scratch"))?;
        let data = self
            .sst_arena
            .alloc_slice_with(crate::store::MAX_PAYLOAD, |_| 0u8)
            .map_err(|_| sql_err!(SQLSTATE_IO, "value-index merge data scratch"))?;
        let bounce = self
            .sst_arena
            .alloc_slice_with(crate::store::MAX_PAYLOAD, |_| 0u8)
            .map_err(|_| sql_err!(SQLSTATE_IO, "value-index merge bounce scratch"))?;
        let mut right_cursor = right.cursor();
        let right_len = right_cursor
            .next_copy(
                &mut *self.blocks.lock().expect("block store lock poisoned"),
                index,
                data,
                bounce,
                &mut self.value_entry,
            )
            .map_err(sst_to_sql)?
            .map_or(0, |(_, length)| length);
        right_cursor.detach_buffer();
        self.slice_writer.reset();
        job.phase = ValueSchedulePhase::Merge {
            destination,
            right_cursor,
            right_len,
            rows: left.rows().checked_add(right.rows()).ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "external query row count exhausted"
                )
            })?,
            output: 1,
        };
        Ok(())
    }

    #[expect(clippy::too_many_arguments, reason = "retained merge cursor state")]
    fn value_merge_beat(
        &mut self,
        storage: &Storage,
        job: &mut ValueScheduleJob,
        destination: ValueMergeDestination,
        mut right_cursor: SstCursor,
        mut right_len: usize,
        rows: u64,
        mut output: u64,
        before: crate::store::BlockIoStats,
    ) -> Result<(), SqlError> {
        self.sst_arena.reset();
        let index = self
            .sst_arena
            .alloc_slice_with(crate::store::MAX_PAYLOAD, |_| 0u8)
            .map_err(|_| sql_err!(SQLSTATE_IO, "value-index merge index scratch"))?;
        let data = self
            .sst_arena
            .alloc_slice_with(crate::store::MAX_PAYLOAD, |_| 0u8)
            .map_err(|_| sql_err!(SQLSTATE_IO, "value-index merge data scratch"))?;
        let bounce = self
            .sst_arena
            .alloc_slice_with(crate::store::MAX_PAYLOAD, |_| 0u8)
            .map_err(|_| sql_err!(SQLSTATE_IO, "value-index merge bounce scratch"))?;
        let mut processed = 0usize;
        loop {
            let left = self.value_sort_reader.prefixed_row();
            let right = (right_len != 0).then_some(&self.value_entry[..right_len]);
            let choose_left = match (left, right) {
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (Some(left), Some(right)) => {
                    let order =
                        Self::compare_value_sort_entries(storage, job, &left[8..], &right[8..])?
                            .then_with(|| {
                                u64::from_le_bytes(left[..8].try_into().unwrap())
                                    .cmp(&u64::from_le_bytes(right[..8].try_into().unwrap()))
                            });
                    !order.is_gt()
                }
                (None, None) => break,
            };
            if choose_left {
                let row = self
                    .value_sort_reader
                    .prefixed_row()
                    .expect("chosen left row");
                self.slice_writer
                    .append(&mut self.block_output, output, row)
                    .map_err(sst_to_sql)?;
                self.value_sort_reader
                    .advance(&mut *self.blocks.lock().expect("block store lock poisoned"))?;
            } else {
                self.slice_writer
                    .append(
                        &mut self.block_output,
                        output,
                        &self.value_entry[..right_len],
                    )
                    .map_err(sst_to_sql)?;
                right_len = right_cursor
                    .next_copy(
                        &mut *self.blocks.lock().expect("block store lock poisoned"),
                        index,
                        data,
                        bounce,
                        &mut self.value_entry,
                    )
                    .map_err(sst_to_sql)?
                    .map_or(0, |(_, length)| length);
            }
            processed += 1;
            output += 1;
            let io = self
                .blocks
                .lock()
                .expect("block store lock poisoned")
                .io_stats()
                .saturating_sub(before);
            if processed >= VALUE_INDEX_WRITE_BEAT_ENTRIES
                || !self.block_output.is_empty()
                || io.object_gets >= VALUE_INDEX_SCHEDULE_BEAT_READS
            {
                right_cursor.detach_buffer();
                job.phase = ValueSchedulePhase::Merge {
                    destination,
                    right_cursor,
                    right_len,
                    rows,
                    output,
                };
                return Ok(());
            }
        }
        if !self.block_output.is_empty()
            || self
                .blocks
                .lock()
                .expect("block store lock poisoned")
                .io_stats()
                .saturating_sub(before)
                .object_gets
                != 0
        {
            right_cursor.detach_buffer();
            job.phase = ValueSchedulePhase::Merge {
                destination,
                right_cursor,
                right_len,
                rows,
                output,
            };
            return Ok(());
        }
        let handle = self
            .slice_writer
            .finish(&mut self.block_output)
            .map_err(sst_to_sql)?
            .expect("merged value-index run is non-empty");
        job.phase = ValueSchedulePhase::CompleteRun {
            run: ExternalRun::from_checkpoint(handle, rows),
            destination,
        };
        Ok(())
    }

    fn value_finish_beat(
        &mut self,
        job: &mut ValueScheduleJob,
        mut accumulator: Option<ExternalRun>,
        mut level: usize,
    ) -> Option<ValueIndexSource> {
        while level < job.levels.len() {
            let Some(run) = job.levels[level].take() else {
                level += 1;
                continue;
            };
            if let Some(left) = accumulator {
                job.phase = ValueSchedulePhase::StartMerge {
                    left,
                    right: run,
                    destination: ValueMergeDestination::Finish(level + 1),
                };
                return None;
            }
            accumulator = Some(run);
            level += 1;
        }
        Some(match accumulator {
            Some(run) => ValueIndexSource::External {
                run,
                started: false,
            },
            None => ValueIndexSource::InMemory {
                rows: 0,
                position: 0,
            },
        })
    }

    fn finish_value_schedule(&mut self, job: ValueScheduleJob, source: ValueIndexSource) {
        self.value_source_cursor.reset();
        self.value_changed_rowids.sort_unstable();
        self.value_changed_rowids.dedup();
        if let Some(spec) = job.navigation {
            self.value_writer.reset_navigation(
                spec.position,
                spec.kind,
                !job.include_mask.is_empty() && spec.kind != crate::store::NavigationKind::Posting,
            );
        } else {
            self.value_writer.reset();
        }
        if let Some(base) = job.base {
            self.value_base_stream.reset(base, job.navigation.is_some());
        }
        self.value_job = Some(ValueIndexJob {
            phase: ValueIndexPhase::Write,
            slot: job.slot,
            table_created_at: job.table_created_at,
            binding: job.binding,
            columns: job.columns,
            n_columns: job.n_columns,
            index_created_at: job.index_created_at,
            include_mask: job.include_mask,
            navigation: job.navigation,
            dirty_lsn: job.dirty_lsn,
            through_lsn: job.through_lsn,
            base: job.base,
            base_len: 0,
            base_done: job.base.is_none(),
            source,
            previous_posting: None,
        });
    }

    fn value_index_schedule_beat(&mut self, storage: &Storage) -> Result<(), SqlError> {
        let mut job = self
            .value_schedule_job
            .take()
            .expect("checked by checkpoint scheduler");
        if !Self::value_schedule_job_is_current(storage, &job) {
            self.slice_writer.reset();
            self.value_source_cursor.reset();
            self.roster_scratch.clear();
            self.value_changed_rowids.clear();
            return Ok(());
        }
        self.index_output_owner = Some(IndexOutputOwner::Schedule);
        #[cfg(feature = "checkpoint-profile")]
        let started = checkpoint_profile_start();
        let before = self
            .blocks
            .lock()
            .expect("block store lock poisoned")
            .io_stats();
        let phase = core::mem::replace(&mut job.phase, ValueSchedulePhase::Collect);
        let result = match phase {
            ValueSchedulePhase::Collect => self.value_collect_beat(storage, &mut job),
            ValueSchedulePhase::WriteChunk { position } => self
                .value_write_chunk_beat(&mut job, position)
                .map(|()| None),
            ValueSchedulePhase::StartMerge {
                left,
                right,
                destination,
            } => self
                .start_value_merge(&mut job, left, right, destination)
                .map(|()| None),
            ValueSchedulePhase::Merge {
                destination,
                right_cursor,
                right_len,
                rows,
                output,
            } => self
                .value_merge_beat(
                    storage,
                    &mut job,
                    destination,
                    right_cursor,
                    right_len,
                    rows,
                    output,
                    before,
                )
                .map(|()| None),
            ValueSchedulePhase::Finish { accumulator, level } => {
                Ok(self.value_finish_beat(&mut job, accumulator, level))
            }
            ValueSchedulePhase::CompleteRun { run, destination } => {
                Self::install_value_run(&mut job, run, destination).map(|()| None)
            }
        };
        #[cfg(feature = "checkpoint-profile")]
        profile_checkpoint_phase(
            "value_index_schedule",
            job.through_lsn,
            Some(job.slot),
            started,
            before,
            self.blocks
                .lock()
                .expect("block store lock poisoned")
                .io_stats(),
            0,
        );
        match result {
            Ok(Some(source)) => self.finish_value_schedule(job, source),
            Ok(None) => self.value_schedule_job = Some(job),
            Err(error) => {
                self.block_output
                    .discard(&mut self.block_uploads)
                    .map_err(upload_to_sql)?;
                self.slice_writer.reset();
                self.value_source_cursor.reset();
                self.value_changed_rowids.clear();
                job.phase = ValueSchedulePhase::Collect;
                job.source_done = false;
                job.pending = None;
                job.chunk_len = 0;
                job.row_count = 0;
                job.next_ordinal = 0;
                job.levels.fill(None);
                self.value_schedule_job = Some(job);
                return Err(error);
            }
        }
        Ok(())
    }

    fn value_job_is_current(storage: &Storage, job: &ValueIndexJob) -> bool {
        if job.slot >= storage.physical_table_count()
            || !storage.table(job.slot).live()
            || storage.table(job.slot).created_at() != job.table_created_at
            || job.binding >= storage.value_binding_count(job.slot)
            || !storage.value_binding_is_committed(job.slot, job.binding)
        {
            return false;
        }
        let (columns, n_columns) = storage.value_binding_columns(job.slot, job.binding);
        job.n_columns == n_columns
            && job.columns[..n_columns] == columns[..n_columns]
            && job.index_created_at == storage.value_binding_created_at(job.slot, job.binding)
            && job.include_mask == storage.value_binding_include_mask(job.slot, job.binding)
            && job.navigation == storage.value_binding_navigation(job.slot, job.binding)
            && storage.value_binding_dirty_lsn(job.slot, job.binding) == Some(job.dirty_lsn)
    }

    fn reset_value_index_output(&mut self, job: &mut ValueIndexJob) {
        if let Some(spec) = job.navigation {
            self.value_writer.reset_navigation(
                spec.position,
                spec.kind,
                !job.include_mask.is_empty() && spec.kind != crate::store::NavigationKind::Posting,
            );
        } else {
            self.value_writer.reset();
        }
        job.phase = ValueIndexPhase::Write;
        job.previous_posting = None;
        match &mut job.source {
            ValueIndexSource::InMemory { position, .. } => *position = 0,
            ValueIndexSource::External { started, .. } => *started = false,
        }
        job.base_len = 0;
        job.base_done = job.base.is_none();
        if let Some(base) = job.base {
            self.value_base_stream.reset(base, job.navigation.is_some());
        }
    }

    /// Streams a sorted value-index generation through a bounded number of
    /// entries and block transfers. A failed beat restarts from the retained
    /// sorted source; content-addressed writes make the replay idempotent.
    fn value_index_write_beat(&mut self, storage: &Storage) -> Result<(), SqlError> {
        let mut job = self.value_job.take().expect("checked by caller");
        if !Self::value_job_is_current(storage, &job) {
            self.value_writer.reset();
            self.roster_scratch.clear();
            self.value_changed_rowids.clear();
            return Ok(());
        }
        #[cfg(feature = "checkpoint-profile")]
        let started = checkpoint_profile_start();
        let before = self
            .blocks
            .lock()
            .expect("block store lock poisoned")
            .io_stats();
        self.index_output_owner = Some(IndexOutputOwner::Index);
        let result = self.write_value_index_entries(storage, &mut job, before);
        #[cfg(feature = "checkpoint-profile")]
        profile_checkpoint_phase(
            "value_index_write",
            job.through_lsn,
            Some(job.slot),
            started,
            before,
            self.blocks
                .lock()
                .expect("block store lock poisoned")
                .io_stats(),
            0,
        );
        match result {
            Ok(Some(handle)) => {
                if self.pending_value_installs.len() == self.pending_value_installs.capacity() {
                    self.reset_value_index_output(&mut job);
                    self.value_job = Some(job);
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "checkpoint value-index install roster is full"
                    ));
                }
                self.pending_value_installs.push(ValueInstall {
                    slot: job.slot,
                    table_created_at: job.table_created_at,
                    columns: job.columns,
                    n_columns: job.n_columns,
                    index_created_at: job.index_created_at,
                    include_mask: job.include_mask,
                    dirty_lsn: job.dirty_lsn,
                    handle,
                });
                self.roster_scratch.clear();
                self.value_changed_rowids.clear();
                Ok(())
            }
            Ok(None) => {
                self.value_job = Some(job);
                Ok(())
            }
            Err(error) => {
                self.block_output
                    .discard(&mut self.block_uploads)
                    .map_err(upload_to_sql)?;
                self.reset_value_index_output(&mut job);
                self.value_job = Some(job);
                Err(error)
            }
        }
    }

    fn write_value_index_entries(
        &mut self,
        storage: &Storage,
        job: &mut ValueIndexJob,
        before: crate::store::BlockIoStats,
    ) -> Result<Option<Option<ValueIndexHandle>>, SqlError> {
        if let ValueIndexPhase::Finish(handle) = job.phase {
            return Ok(Some(handle));
        }
        if let ValueIndexSource::External { run, started } = &mut job.source
            && !*started
        {
            self.value_sort_reader.start(
                &mut *self.blocks.lock().expect("block store lock poisoned"),
                *run,
            )?;
            *started = true;
        }
        let mut processed = 0usize;
        loop {
            let delta_len = match &job.source {
                ValueIndexSource::InMemory { rows, position } if position < rows => {
                    let row = self.value_sort_rows[*position];
                    let start = row.offset as usize + 8;
                    let end = row.offset as usize + row.length as usize;
                    let entry_len = end - start;
                    self.value_entry[..entry_len].copy_from_slice(&self.value_source[start..end]);
                    entry_len
                }
                ValueIndexSource::External { .. } => match self.value_sort_reader.row() {
                    Some(entry) => {
                        self.value_entry[..entry.len()].copy_from_slice(entry);
                        entry.len()
                    }
                    None => 0,
                },
                ValueIndexSource::InMemory { .. } => 0,
            };
            while job.base_len == 0 && !job.base_done {
                let io = self
                    .blocks
                    .lock()
                    .expect("block store lock poisoned")
                    .io_stats()
                    .saturating_sub(before);
                let reserve_delta_read =
                    usize::from(matches!(job.source, ValueIndexSource::External { .. }));
                let remaining = (VALUE_INDEX_WRITE_BEAT_READS.saturating_sub(io.object_gets))
                    .saturating_sub(reserve_delta_read as u64)
                    as usize;
                let step = self
                    .value_base_stream
                    .next_copy(
                        &mut *self.blocks.lock().expect("block store lock poisoned"),
                        remaining,
                        &mut self.value_base_entry,
                    )
                    .map_err(value_index_to_sql)?;
                match step {
                    ValueIndexStreamStep::Pending => return Ok(None),
                    ValueIndexStreamStep::Done => job.base_done = true,
                    ValueIndexStreamStep::Entry(length) => {
                        let rowid =
                            u64::from_le_bytes(self.value_base_entry[8..16].try_into().unwrap());
                        if self.value_changed_rowids.binary_search(&rowid).is_ok() {
                            continue;
                        }
                        job.base_len = length;
                    }
                }
            }
            if delta_len == 0 && job.base_len == 0 {
                let handle = {
                    let mut published = PublishedValueBlockWriter {
                        inner: &mut self.block_output,
                        known_blocks: &self.roster_scratch,
                    };
                    self.value_writer
                        .finish(&mut published, job.through_lsn)
                        .map_err(value_index_to_sql)?
                };
                job.phase = ValueIndexPhase::Finish(handle);
                return Ok(None);
            }

            let take_base = match (job.base_len, delta_len) {
                (0, _) => false,
                (_, 0) => true,
                (base_len, delta_len) => {
                    let base = &self.value_base_entry[..base_len];
                    let delta = &self.value_entry[..delta_len];
                    Self::compare_value_entries(
                        storage,
                        job.slot,
                        job.binding,
                        job.navigation,
                        base,
                        delta,
                    )?
                    .then_with(|| base[8..16].cmp(&delta[8..16]))
                    .then_with(|| base[16..24].cmp(&delta[16..24]))
                    .is_le()
                }
            };
            let entry_len = if take_base {
                self.value_entry[..job.base_len]
                    .copy_from_slice(&self.value_base_entry[..job.base_len]);
                job.base_len
            } else {
                delta_len
            };

            let entry = &self.value_entry[..entry_len];
            let key = value_sort_key(entry)?;
            let payload = value_sort_payload(entry)?;
            let hash = u64::from_le_bytes(entry[..8].try_into().unwrap());
            let rowid = u64::from_le_bytes(entry[8..16].try_into().unwrap());
            let commit_lsn = u64::from_le_bytes(entry[16..24].try_into().unwrap());
            let posting_token = if job
                .navigation
                .is_some_and(|navigation| navigation.kind == crate::store::NavigationKind::Posting)
            {
                Some(
                    crate::sql::index_signature::PostingToken::decode(key).ok_or_else(|| {
                        sql_err!(SQLSTATE_IO, "persistent GIN posting key is corrupt")
                    })?,
                )
            } else {
                None
            };
            let duplicate = posting_token.is_some_and(|token| {
                job.previous_posting == Some((token.encode(), rowid, commit_lsn))
            });
            if !duplicate {
                if let Some(token) = posting_token {
                    job.previous_posting = Some((token.encode(), rowid, commit_lsn));
                }
                let mut comparison_error = None;
                let mut compare_keys = |left: &[u8], right: &[u8]| match storage
                    .compare_value_binding_keys(
                        0,
                        job.slot,
                        job.binding,
                        job.navigation,
                        left,
                        right,
                    ) {
                    Ok(ordering) => ordering,
                    Err(error) => {
                        comparison_error = Some(error);
                        core::cmp::Ordering::Equal
                    }
                };
                let summary = if let Some(spec) = job.navigation {
                    Some(posting_token.map_or_else(
                        || {
                            storage.value_binding_navigation_summary(
                                job.slot,
                                job.binding,
                                spec,
                                key,
                            )
                        },
                        |token| Ok(token.summary()),
                    )?)
                } else {
                    None
                };
                let write = {
                    let mut published = PublishedValueBlockWriter {
                        inner: &mut self.block_output,
                        known_blocks: &self.roster_scratch,
                    };
                    if let Some(spec) = job.navigation {
                        self.value_writer.append_navigation(
                            &mut published,
                            (hash, rowid, commit_lsn),
                            key,
                            (!job.include_mask.is_empty()
                                && spec.kind != crate::store::NavigationKind::Posting)
                                .then_some(payload),
                            summary.expect("navigation summary was computed"),
                            &mut compare_keys,
                        )
                    } else if job.include_mask.is_empty() {
                        self.value_writer.append(
                            &mut published,
                            hash,
                            rowid,
                            commit_lsn,
                            key,
                            &mut compare_keys,
                        )
                    } else {
                        self.value_writer.append_covering(
                            &mut published,
                            (hash, rowid, commit_lsn),
                            key,
                            payload,
                            &mut compare_keys,
                        )
                    }
                };
                write.map_err(value_index_to_sql)?;
                if let Some(error) = comparison_error {
                    return Err(error);
                }
            }

            if take_base {
                job.base_len = 0;
            } else {
                match &mut job.source {
                    ValueIndexSource::InMemory { position, .. } => *position += 1,
                    ValueIndexSource::External { .. } => self
                        .value_sort_reader
                        .advance(&mut *self.blocks.lock().expect("block store lock poisoned"))?,
                }
            }
            processed += 1;
            let io = self
                .blocks
                .lock()
                .expect("block store lock poisoned")
                .io_stats()
                .saturating_sub(before);
            if processed >= VALUE_INDEX_WRITE_BEAT_ENTRIES
                || !self.block_output.is_empty()
                || io.object_gets >= VALUE_INDEX_WRITE_BEAT_READS
            {
                return Ok(None);
            }
        }
    }

    /// Mark-and-sweep over `blocks/`: the keep-set is every identity on the
    /// rosters of the SSTs the manifest just published (each roster is one
    /// block read, through the cache), plus the rosters themselves; anything
    /// else under the prefix is an orphan from a superseded checkpoint or an
    /// interrupted write, and is deleted. An undersized keep-set is a loud
    /// error rather than a successful checkpoint that silently retains debt.
    /// Returns true once the namespace is clean; false means another paced
    /// deletion batch remains.
    fn collect_block_garbage_batch(&mut self, storage: &Storage) -> Result<bool, SqlError> {
        if !self.block_garbage_loaded {
            // Durable block writers cannot run while post-publication
            // maintenance is pending. Foreground statements only mutate RAM,
            // and temporary spill uses its own local store, so a staged orphan
            // cannot become live before this batch drains.
            #[cfg(feature = "checkpoint-profile")]
            let keep_started = checkpoint_profile_start();
            #[cfg(feature = "checkpoint-profile")]
            let keep_before = self
                .blocks
                .lock()
                .expect("block store lock poisoned")
                .io_stats();
            self.ensure_backup_names_loaded()?;
            {
                let mut keep = BlockKeepSet::new(&mut self.roster_scratch, &mut self.garbage_seen);
                self.sst_arena.reset();
                let scratch = self
                    .sst_arena
                    .alloc_slice_with(crate::store::MAX_PAYLOAD, |_| 0u8)
                    .map_err(|_| sql_err!(SQLSTATE_IO, "gc scratch"))?;
                // A merge mid-flight has written blocks no published roster names
                // yet; sweeping them would destroy the job's progress.
                if self.merge_job.is_some() {
                    for id in self.merge_writer.roster_so_far() {
                        keep.insert(*id)?;
                    }
                }
                storage.copy_retained_spill_roots(true, &mut self.garbage_sst_roots)?;
                // A completed merge can finish after another manifest was
                // captured. It still owns its graph until a later publication
                // installs it or its source pair is replaced.
                let completed = self
                    .merge_done
                    .iter()
                    .filter_map(|done| done.as_ref()?.merged);
                let finishing = self.merge_job.as_ref().and_then(|job| match job.phase {
                    MergePhase::Finish { merged } => merged,
                    _ => None,
                });
                for prev in self
                    .prev_ssts
                    .iter()
                    .flat_map(SlotList::iter)
                    .copied()
                    .chain(completed)
                    .chain(finishing)
                {
                    if self.garbage_sst_roots.len() == self.garbage_sst_roots.capacity() {
                        return Err(sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "garbage collection SST roots exceed fixed scratch capacity"
                        ));
                    }
                    self.garbage_sst_roots.push(prev.handle);
                }
                self.garbage_sst_roots
                    .sort_unstable_by_key(|handle| handle.roster);
                self.garbage_sst_roots.dedup_by_key(|handle| handle.roster);
                for h in self.garbage_sst_roots.iter().copied() {
                    keep.insert(h.roster)?;
                    let n = self
                        .blocks
                        .lock()
                        .expect("block store lock poisoned")
                        .get(&h.roster, scratch)
                        .map(|(n, _)| n)
                        .map_err(|e| {
                            sql_err!(SQLSTATE_IO, "gc roster {:?} read: {:?}", h.roster, e)
                        })?;
                    for id_bytes in scratch[..n].chunks(32) {
                        if id_bytes.len() != 32 {
                            return Err(sql_err!(
                                SQLSTATE_IO,
                                "gc roster is not a multiple of 32 bytes"
                            ));
                        }

                        let mut id = [0u8; 32];
                        id.copy_from_slice(id_bytes);
                        keep.insert(BlockId(id))?;
                    }
                }
                storage.copy_retained_index_roots(&mut self.garbage_index_roots)?;
                for slot in 0..storage.physical_table_count() {
                    for binding in 0..storage.value_binding_count(slot) {
                        let Some(handle) = storage.value_binding_handle(slot, binding) else {
                            continue;
                        };
                        if self.garbage_index_roots.contains(&handle.roster) {
                            continue;
                        }
                        if self.garbage_index_roots.len() == self.garbage_index_roots.capacity() {
                            return Err(sql_err!(
                                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                "garbage collection index roots exceed fixed scratch capacity"
                            ));
                        }
                        self.garbage_index_roots.push(handle.roster);
                    }
                }
                self.garbage_index_roots.sort_unstable();
                self.garbage_index_roots.dedup();
                for roster in self.garbage_index_roots.iter().copied() {
                    let complete = crate::store::walk_value_roster(
                        &mut *self.blocks.lock().expect("block store lock poisoned"),
                        roster,
                        scratch,
                        |id, _| keep.insert(id).is_ok(),
                    )
                    .map_err(|error| {
                        sql_err!(
                            SQLSTATE_IO,
                            "corrupt persistent value-index roster: {:?}",
                            error
                        )
                    })?;
                    if !complete {
                        return Err(sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "block garbage collection keep-set exceeds checkpoint_live_blocks ({})",
                            keep.capacity()
                        ));
                    }
                }
                // A delayed publication can name an index no longer installed
                // in a rewritten or dropped live table. Recovery still needs
                // every graph in the durable root before replaying its suffix.
                if self.manifest_present {
                    self.client
                        .get(MANIFEST_KEY, None)
                        .map_err(object_store_to_sql)?;
                    self.manifest_buf.clear();
                    if !self.manifest_buf.append(self.client.body_bytes()) {
                        return Err(manifest_full());
                    }
                    retain_manifest_blocks(
                        &self.blocks,
                        &mut keep,
                        scratch,
                        self.manifest_buf.readable(),
                    )?;
                }
                for index in 0..self.backup_scratch.len() {
                    let name = self.backup_scratch[index];
                    let key = stack_format!(128, "backups/{}/manifest", name.as_str());
                    self.client
                        .get(key.as_str(), None)
                        .map_err(object_store_to_sql)?;
                    self.manifest_buf.clear();
                    if !self.manifest_buf.append(self.client.body_bytes()) {
                        return Err(manifest_full());
                    }
                    retain_manifest_blocks(
                        &self.blocks,
                        &mut keep,
                        scratch,
                        self.manifest_buf.readable(),
                    )?;
                }
            }
            // Listing may visit many obsolete objects. Sort the fixed keep-set
            // once so each membership probe is logarithmic rather than scanning
            // every live block for every listed key.
            self.roster_scratch.sort_unstable_by_key(|(id, _)| *id);
            #[cfg(feature = "checkpoint-profile")]
            profile_checkpoint_phase(
                "gc_keep",
                storage.lsn(),
                None,
                keep_started,
                keep_before,
                self.blocks
                    .lock()
                    .expect("block store lock poisoned")
                    .io_stats(),
                0,
            );
            self.doomed_blocks.clear();
            let keep = &self.roster_scratch;
            let doomed = &mut self.doomed_blocks;
            let mut overflow = false;
            #[cfg(feature = "checkpoint-profile")]
            let list_started = checkpoint_profile_start();
            #[cfg(feature = "checkpoint-profile")]
            let list_before = self
                .blocks
                .lock()
                .expect("block store lock poisoned")
                .io_stats();
            if let Err(error) = self.client.list("blocks/", |key| {
                let hex = key.strip_prefix("blocks/").unwrap_or(key);
                let known = parse_block_id(hex)
                    .map(|id| keep.binary_search_by_key(&id, |(known, _)| *known).is_ok())
                    .unwrap_or(false);
                if !known {
                    if doomed.len() < doomed.capacity() {
                        doomed.push(crate::stack_format!(80, "{}", key));
                    } else {
                        overflow = true;
                    }
                }
            }) {
                self.doomed_blocks.clear();
                return Err(object_store_to_sql(error));
            }
            #[cfg(feature = "checkpoint-profile")]
            profile_checkpoint_phase(
                "gc_list",
                storage.lsn(),
                None,
                list_started,
                list_before,
                self.blocks
                    .lock()
                    .expect("block store lock poisoned")
                    .io_stats(),
                0,
            );
            self.block_garbage_overflow = overflow;
            self.block_garbage_loaded = true;
        }
        #[cfg(feature = "checkpoint-profile")]
        let delete_started = checkpoint_profile_start();
        #[cfg(feature = "checkpoint-profile")]
        let delete_before = self
            .blocks
            .lock()
            .expect("block store lock poisoned")
            .io_stats();
        #[cfg(feature = "checkpoint-profile")]
        let mut deleted = 0;
        for _ in 0..self.delete_objects_per_beat.min(self.doomed_blocks.len()) {
            let key = *self
                .doomed_blocks
                .last()
                .expect("nonempty staged block deletion batch");
            self.client
                .delete(key.as_str())
                .map_err(object_store_to_sql)?;
            self.doomed_blocks.pop();
            #[cfg(feature = "checkpoint-profile")]
            {
                deleted += 1;
            }
        }
        let done = if self.doomed_blocks.is_empty() {
            self.block_garbage_loaded = false;
            let done = !self.block_garbage_overflow;
            self.block_garbage_overflow = false;
            done
        } else {
            false
        };
        #[cfg(feature = "checkpoint-profile")]
        profile_checkpoint_phase(
            "gc_delete",
            storage.lsn(),
            None,
            delete_started,
            delete_before,
            self.blocks
                .lock()
                .expect("block store lock poisoned")
                .io_stats(),
            deleted,
        );
        if done && storage.take_retained_file_reclamation() {
            return Ok(false);
        }
        Ok(done)
    }

    /// Removes objects from the obsolete pre-content-addressed SST namespace.
    /// Current manifests reference only content-addressed `blocks/` objects.
    /// Returns true once the namespace is clean; false means another paced
    /// deletion batch remains.
    fn collect_garbage_batch(&mut self) -> Result<bool, SqlError> {
        #[cfg(feature = "checkpoint-profile")]
        let started = checkpoint_profile_start();
        #[cfg(feature = "checkpoint-profile")]
        let before = self
            .blocks
            .lock()
            .expect("block store lock poisoned")
            .io_stats();
        // Retain the fixed staging batch across paced delete beats so a large
        // legacy namespace is listed once per staging batch.
        if !self.legacy_garbage_loaded {
            self.garbage_scratch.clear();
            let doomed = &mut self.garbage_scratch;
            let mut overflow = false;
            if let Err(error) = self.client.list("sst/", |key| {
                if doomed.len() < doomed.capacity() {
                    doomed.push(crate::stack_format!(64, "{}", key));
                } else {
                    overflow = true;
                }
            }) {
                self.garbage_scratch.clear();
                return Err(object_store_to_sql(error));
            }
            self.legacy_garbage_overflow = overflow;
            self.legacy_garbage_loaded = true;
        }
        #[cfg(feature = "checkpoint-profile")]
        let mut deleted = 0;
        for _ in 0..self.delete_objects_per_beat.min(self.garbage_scratch.len()) {
            let key = *self
                .garbage_scratch
                .last()
                .expect("nonempty staged legacy deletion batch");
            self.client
                .delete(key.as_str())
                .map_err(object_store_to_sql)?;
            self.garbage_scratch.pop();
            #[cfg(feature = "checkpoint-profile")]
            {
                deleted += 1;
            }
        }
        let done = if self.garbage_scratch.is_empty() {
            self.legacy_garbage_loaded = false;
            let done = !self.legacy_garbage_overflow;
            self.legacy_garbage_overflow = false;
            done
        } else {
            false
        };
        #[cfg(feature = "checkpoint-profile")]
        profile_checkpoint_phase(
            "legacy_gc",
            self.manifest_lsn,
            None,
            started,
            before,
            self.blocks
                .lock()
                .expect("block store lock poisoned")
                .io_stats(),
            deleted,
        );
        Ok(done)
    }
}

/// Reconstructs a local-journal frame from an uploaded record which starts at
/// the kind byte. Object replay deliberately hands the decoder that compact
/// suffix; logical streaming needs the complete journal frame expected by the
/// common committed-transaction cursor.
fn append_uploaded_wal_record(
    scratch: &mut FixedBuf,
    lsn: u64,
    record: &[u8],
) -> Result<(), SqlError> {
    let payload_len = record
        .len()
        .checked_sub(8)
        .ok_or_else(|| sql_err!(sqlstate::PROTOCOL_VIOLATION, "corrupt uploaded WAL record"))?;
    let total = crate::wal::HEADER_LEN + payload_len;
    if scratch.capacity() - scratch.len() < total {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "one committed WAL transaction exceeds replication buffer"
        ));
    }
    let mark = scratch.mark();
    assert!(scratch.append(&[0u8; 16]));
    assert!(scratch.append(record));
    let filled = scratch.filled_mut();
    filled[mark + 4..mark + 8].copy_from_slice(&(payload_len as u32).to_le_bytes());
    filled[mark + 8..mark + 16].copy_from_slice(&lsn.to_le_bytes());
    let crc = crate::wal::crc32c::crc32c(&filled[mark + 4..mark + total]);
    filled[mark..mark + 4].copy_from_slice(&crc.to_le_bytes());
    Ok(())
}

fn parse_hex_array<const N: usize>(hex: &str) -> Result<[u8; N], CheckpointSetupError> {
    let (output, len) = parse_hex_bytes(hex)?;
    if len != N {
        return Err(CheckpointSetupError::Corrupt(
            "fixed byte field has the wrong hex length",
        ));
    }
    Ok(output)
}

fn parse_hex_bytes<const N: usize>(hex: &str) -> Result<([u8; N], usize), CheckpointSetupError> {
    let bytes = hex.as_bytes();
    if !bytes.len().is_multiple_of(2) || bytes.len() > 2 * N {
        return Err(CheckpointSetupError::Corrupt(
            "fixed byte field has the wrong hex length",
        ));
    }
    let nibble = |b: u8| -> Result<u8, CheckpointSetupError> {
        match b {
            b'0'..=b'9' => Ok(b - b'0'),
            b'a'..=b'f' => Ok(b - b'a' + 10),
            _ => Err(CheckpointSetupError::Corrupt(
                "block id is not lowercase hex",
            )),
        }
    };
    let mut output = [0u8; N];
    for (i, pair) in bytes.chunks(2).enumerate() {
        output[i] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Ok((output, bytes.len() / 2))
}

fn parse_hex_salt(hex: &str) -> Result<crate::pg::auth::ScramSalt, CheckpointSetupError> {
    if hex == "-" {
        return Ok(crate::pg::auth::ScramSalt::EMPTY);
    }
    let (bytes, len) = parse_hex_bytes::<{ crate::pg::auth::SCRAM_SALT_MAX }>(hex)?;
    crate::pg::auth::ScramSalt::from_bytes(&bytes[..len])
        .ok_or(CheckpointSetupError::Corrupt("invalid SCRAM salt"))
}

fn parse_block_id(hex: &str) -> Result<BlockId, CheckpointSetupError> {
    Ok(BlockId(parse_hex_array(hex)?))
}

fn parse_dsst_handle<'a>(
    index: &str,
    filter: &str,
    roster: &str,
    words: &mut impl Iterator<Item = &'a str>,
) -> Result<Option<SstHandle>, CheckpointSetupError> {
    if index == "-" {
        if filter != "-" || roster != "-" || words.next().is_some() {
            return Err(CheckpointSetupError::Corrupt("malformed empty dsst"));
        }
        return Ok(None);
    }
    if filter == "-" || roster == "-" {
        return Err(CheckpointSetupError::Corrupt("incomplete dsst handle"));
    }
    let format = words
        .next()
        .and_then(RowSstFormat::from_manifest_id)
        .ok_or(CheckpointSetupError::Corrupt("unknown dsst format"))?;
    if words.next().is_some() {
        return Err(CheckpointSetupError::Corrupt("malformed dsst handle"));
    }
    Ok(Some(SstHandle {
        index: parse_block_id(index)?,
        filter: parse_block_id(filter)?,
        roster: parse_block_id(roster)?,
        format,
    }))
}

fn manifest_lsn(bytes: &[u8]) -> Result<u64, &'static str> {
    let text = core::str::from_utf8(bytes).map_err(|_| "manifest is not UTF-8")?;
    Ok(parse_manifest_metadata(text)?.lsn)
}

fn manifest_commit_retention_floor(bytes: &[u8]) -> Result<u64, &'static str> {
    let mut floor = manifest_lsn(bytes)?;
    let text = core::str::from_utf8(bytes).map_err(|_| "manifest is not UTF-8")?;
    for line in text.lines().skip(1) {
        let mut words = line.split(' ');
        match words.next() {
            Some("ptx") => {
                words
                    .next()
                    .ok_or("prepared transaction identity missing")?;
                let first_lsn: u64 = words
                    .next()
                    .and_then(|word| word.parse().ok())
                    .filter(|lsn| *lsn != 0)
                    .ok_or("prepared transaction first LSN is invalid")?;
                floor = floor.min(first_lsn.saturating_sub(1));
            }
            Some("rslot") => {
                words.next().ok_or("replication slot name missing")?;
                let restart_lsn: u64 = words
                    .next()
                    .and_then(|word| word.parse().ok())
                    .ok_or("replication slot restart LSN is invalid")?;
                floor = floor.min(restart_lsn);
            }
            _ => {}
        }
    }
    Ok(floor)
}

struct BlockKeepSet<'scratch> {
    blocks: &'scratch mut Vec<(BlockId, Option<BlockType>)>,
    seen: &'scratch mut FixedMap<BlockId, ()>,
}

impl<'scratch> BlockKeepSet<'scratch> {
    fn new(
        blocks: &'scratch mut Vec<(BlockId, Option<BlockType>)>,
        seen: &'scratch mut FixedMap<BlockId, ()>,
    ) -> Self {
        assert_eq!(
            blocks.capacity(),
            seen.capacity(),
            "keep-set reservations agree"
        );
        blocks.clear();
        seen.clear();
        Self { blocks, seen }
    }

    fn capacity(&self) -> usize {
        self.blocks.capacity()
    }

    fn insert(&mut self, id: BlockId) -> Result<(), SqlError> {
        match self.seen.insert(id, ()) {
            Ok(None) => {
                self.blocks.push((id, None));
                Ok(())
            }
            Ok(Some(())) => Ok(()),
            Err(_) => Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "block garbage collection keep-set exceeds checkpoint_live_blocks ({})",
                self.capacity()
            )),
        }
    }
}

fn retain_block(keep: &mut BlockKeepSet<'_>, id: BlockId) -> Result<(), SqlError> {
    keep.insert(id)
}

fn retain_manifest_blocks(
    blocks: &std::sync::Arc<Mutex<TieredStore<OwnedObjectStore>>>,
    keep: &mut BlockKeepSet<'_>,
    scratch: &mut [u8],
    bytes: &[u8],
) -> Result<(), SqlError> {
    let text = core::str::from_utf8(bytes)
        .map_err(|_| sql_err!(SQLSTATE_IO, "saved manifest is not UTF-8"))?;
    parse_manifest_metadata(text)
        .map_err(|message| sql_err!(SQLSTATE_IO, "saved manifest: {message}"))?;
    for line in text.lines().skip(1) {
        let mut words = line.split(' ');
        match words.next() {
            Some("dsst") => {
                for _ in 0..4 {
                    words.next().ok_or_else(|| {
                        sql_err!(SQLSTATE_IO, "saved manifest has a malformed dsst")
                    })?;
                }
                words
                    .next()
                    .ok_or_else(|| sql_err!(SQLSTATE_IO, "saved manifest has a malformed dsst"))?;
                words
                    .next()
                    .ok_or_else(|| sql_err!(SQLSTATE_IO, "saved manifest has a malformed dsst"))?;
                let roster = words
                    .next()
                    .ok_or_else(|| sql_err!(SQLSTATE_IO, "saved manifest has a malformed dsst"))?;
                if roster == "-" {
                    continue;
                }
                let roster = parse_block_id(roster)
                    .map_err(|_| sql_err!(SQLSTATE_IO, "saved manifest has a bad dsst roster"))?;
                retain_block(keep, roster)?;
                let n = blocks
                    .lock()
                    .expect("block store lock poisoned")
                    .get(&roster, scratch)
                    .map(|(n, _)| n)
                    .map_err(|error| sql_err!(SQLSTATE_IO, "saved row roster read: {:?}", error))?;
                for id_bytes in scratch[..n].chunks(32) {
                    if id_bytes.len() != 32 {
                        return Err(sql_err!(
                            SQLSTATE_IO,
                            "saved row roster is not a multiple of 32 bytes"
                        ));
                    }
                    let mut id = [0; 32];
                    id.copy_from_slice(id_bytes);
                    retain_block(keep, BlockId(id))?;
                }
            }
            Some("vix") => {
                words
                    .next()
                    .ok_or_else(|| sql_err!(SQLSTATE_IO, "saved manifest has a malformed vix"))?;
                let column_count: usize = words
                    .next()
                    .and_then(|word| word.parse().ok())
                    .filter(|count| *count != 0 && *count <= crate::storage::MAX_INDEX_COLS)
                    .ok_or_else(|| {
                        sql_err!(SQLSTATE_IO, "saved manifest has a bad vix column count")
                    })?;
                for _ in 0..column_count {
                    words.next().ok_or_else(|| {
                        sql_err!(SQLSTATE_IO, "saved manifest has a malformed vix")
                    })?;
                }
                words
                    .next()
                    .ok_or_else(|| sql_err!(SQLSTATE_IO, "saved manifest has a malformed vix"))?;
                words
                    .next()
                    .ok_or_else(|| sql_err!(SQLSTATE_IO, "saved manifest has a malformed vix"))?;
                let roster = words
                    .next()
                    .ok_or_else(|| sql_err!(SQLSTATE_IO, "saved manifest has a malformed vix"))
                    .and_then(|word| {
                        parse_block_id(word).map_err(|_| {
                            sql_err!(SQLSTATE_IO, "saved manifest has a bad vix roster")
                        })
                    })?;
                let complete = crate::store::walk_value_roster(
                    &mut *blocks.lock().expect("block store lock poisoned"),
                    roster,
                    scratch,
                    |id, _| retain_block(keep, id).is_ok(),
                )
                .map_err(|error| sql_err!(SQLSTATE_IO, "saved value-index roster: {:?}", error))?;
                if !complete {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "block garbage collection keep-set exceeds checkpoint_live_blocks ({})",
                        keep.capacity()
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn sst_to_sql(e: crate::store::SstError) -> SqlError {
    match e {
        crate::store::SstError::Store(crate::store::StoreError::NotReady) => {
            sql_err!(
                crate::sql::eval::sqlstate::INTERNAL_IO_WAIT,
                "block fetch in progress"
            )
        }
        crate::store::SstError::Store(
            crate::store::StoreError::WriteBatchBytesExhausted
            | crate::store::StoreError::WriteBatchObjectsExhausted,
        ) => sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "physical output emission exceeds its startup reservation"
        ),
        crate::store::SstError::Store(crate::store::StoreError::ReadOwnersExhausted) => {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "object read consumers exceed max_connections and max_subscriptions"
            )
        }
        other => sql_err!(SQLSTATE_IO, "checkpoint sst: {:?}", other),
    }
}

fn temporary_sst_to_sql(error: crate::store::SstError) -> SqlError {
    match error {
        crate::store::SstError::Store(crate::store::StoreError::Unavailable) => sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "temporary spill store is full; raise temporary_spill_bytes"
        ),
        other => sql_err!(sqlstate::IO_ERROR, "temporary spill sst: {:?}", other),
    }
}

fn temporary_store_to_sql(error: crate::store::StoreError) -> SqlError {
    match error {
        crate::store::StoreError::Unavailable => sql_err!(
            sqlstate::IO_ERROR,
            "temporary spill store could not compact its local file"
        ),
        other => sql_err!(
            sqlstate::INTERNAL_ERROR,
            "temporary spill store is corrupt: {:?}",
            other
        ),
    }
}

fn value_index_to_sql(error: impl core::fmt::Debug) -> SqlError {
    sql_err!(SQLSTATE_IO, "persistent value-index write: {:?}", error)
}

fn manifest_full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "checkpoint manifest is full; raise checkpoint_manifest_bytes"
    )
}

fn write_manifest(buffer: &mut FixedBuf, line: impl core::fmt::Display) -> Result<(), SqlError> {
    use core::fmt::Write;
    writeln!(buffer, "{line}").map_err(|_| manifest_full())
}

fn write_database_context(
    buffer: &mut FixedBuf,
    current: &mut Option<crate::storage::DatabaseOid>,
    database: crate::storage::DatabaseOid,
) -> Result<(), SqlError> {
    if *current != Some(database) {
        write_manifest(buffer, format_args!("dbctx {}", database.get()))?;
        *current = Some(database);
    }
    Ok(())
}

fn upload_to_sql(error: crate::object_store::upload::UploadError) -> SqlError {
    use crate::object_store::upload::UploadError;
    match error {
        UploadError::Request(error) => object_store_to_sql(error),
        UploadError::AdmissionPaused => sql_err!(
            sqlstate::INTERNAL_IO_WAIT,
            "object requests are draining for credential installation"
        ),
        UploadError::SlotsExhausted => sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "object_store_put_slots exhausted"
        ),
        UploadError::BodyTooLarge {
            requested,
            capacity,
        } => sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "upload needs {} bytes but wal_upload_buffer_bytes is {}",
            requested,
            capacity
        ),
        UploadError::KeyTooLarge => sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "object key exceeds S3 key limit"
        ),
        UploadError::GenerationExhausted => sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "upload ticket generation exhausted"
        ),
        UploadError::InvalidTicket => {
            sql_err!(sqlstate::INTERNAL_ERROR, "invalid object upload ticket")
        }
    }
}

fn object_store_to_sql(e: ObjectError) -> SqlError {
    sql_err!(SQLSTATE_IO, "{}", e)
}

/// Parses framed WAL records from an uploaded segment (same layout as the
/// local journal: crc u32 | len u32 | lsn u64 | payload) and applies each
/// with lsn > floor. Returns the highest LSN seen.
/// Replays the complete records in `bytes`, returning (highest applied LSN,
/// bytes consumed) — a trailing partial record is left for the caller's next
/// window to re-fetch whole.
fn replay_segment_bytes(
    bytes: &[u8],
    floor: u64,
    apply: &mut impl FnMut(u64, &[u8]) -> Result<(), SqlError>,
) -> Result<usize, SqlError> {
    const HEADER_LEN: usize = 24;
    let mut at = 0usize;
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
        }
        at += total;
    }
    Ok(at)
}

#[derive(Debug)]
pub enum CheckpointSetupError {
    Budget(BudgetError),
    ObjectStore(String),
    Corrupt(&'static str),
    Replay(SqlError),
}

impl std::fmt::Display for CheckpointSetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Budget(e) => write!(f, "checkpoint: {e}"),
            Self::ObjectStore(what) => write!(f, "checkpoint: {what}"),
            Self::Corrupt(what) => write!(f, "checkpoint: corrupt bucket state: {what}"),
            Self::Replay(e) => write!(f, "checkpoint: wal replay failed: {}", e.message.as_str()),
        }
    }
}

impl std::error::Error for CheckpointSetupError {}

struct BackupRoots {
    floor: u64,
    created_at: Option<i64>,
    manifest_crc: u32,
    head_crc: u32,
    manifest: FixedBuf,
    head: FixedBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecoveryTarget {
    Backup,
    Lsn(u64),
    Time(i64),
}

#[derive(Clone, Copy)]
struct CommitChainEntry {
    id: CommitBatchId,
    previous: Option<CommitBatchId>,
}

fn load_backup_roots(
    client: &mut ObjectStore,
    budget: &mut Budget,
    config: &Config,
    name: &str,
) -> Result<BackupRoots, CheckpointSetupError> {
    validate_backup_name(name)
        .map_err(|message| CheckpointSetupError::ObjectStore(message.to_string()))?;
    let mut head =
        FixedBuf::new(budget, "backup root", 128).map_err(CheckpointSetupError::Budget)?;
    let complete_key = stack_format!(128, "backups/{name}/complete");
    read_object_into(
        client,
        complete_key.as_str(),
        &mut head,
        "backup completion record",
    )?;
    let (floor, created_at, manifest_crc, head_crc) =
        parse_backup_complete(head.readable()).map_err(CheckpointSetupError::Corrupt)?;

    let mut manifest = FixedBuf::new(budget, "backup manifest", config.checkpoint_manifest_bytes)
        .map_err(CheckpointSetupError::Budget)?;
    let manifest_key = stack_format!(128, "backups/{name}/manifest");
    read_object_into(
        client,
        manifest_key.as_str(),
        &mut manifest,
        "checkpoint_manifest_bytes",
    )?;
    if manifest_lsn(manifest.readable()).map_err(CheckpointSetupError::Corrupt)? != floor
        || crate::wal::crc32c::crc32c(manifest.readable()) != manifest_crc
    {
        return Err(CheckpointSetupError::Corrupt(
            "backup manifest does not match its completion record",
        ));
    }

    let head_key = stack_format!(128, "backups/{name}/commit-head");
    read_object_into(
        client,
        head_key.as_str(),
        &mut head,
        "backup root format bound",
    )?;
    if head.len() > 128 {
        return Err(CheckpointSetupError::Corrupt(
            "backup commit-head exceeds its format bound",
        ));
    }
    core::str::from_utf8(head.readable())
        .map_err(|_| CheckpointSetupError::Corrupt("backup commit-head is not UTF-8"))?;
    if crate::wal::crc32c::crc32c(head.readable()) != head_crc {
        return Err(CheckpointSetupError::Corrupt(
            "backup commit-head does not match its completion record",
        ));
    }
    if !head.is_empty() {
        parse_commit_head(head.readable()).map_err(CheckpointSetupError::Corrupt)?;
    }
    Ok(BackupRoots {
        floor,
        created_at,
        manifest_crc,
        head_crc,
        manifest,
        head,
    })
}

/// Atomically recoverable offline root replacement. The durable pending marker
/// is removed only after both object roots and every authoritative local-cache
/// file have been replaced. Normal startup refuses to cross that marker.
pub(crate) fn restore_backup(
    config: &Config,
    budget: &mut Budget,
    name: &str,
) -> Result<u64, CheckpointSetupError> {
    restore_backup_to(config, budget, name, RecoveryTarget::Backup)
}

pub(crate) fn restore_backup_to(
    config: &Config,
    budget: &mut Budget,
    name: &str,
    target: RecoveryTarget,
) -> Result<u64, CheckpointSetupError> {
    validate_backup_name(name)
        .map_err(|message| CheckpointSetupError::ObjectStore(message.to_string()))?;
    if !config.object_store_on {
        return Err(CheckpointSetupError::ObjectStore(
            "backup restore requires object_store = on".to_string(),
        ));
    }
    let mut client = ObjectStore::new(config, budget)
        .map_err(|error| CheckpointSetupError::ObjectStore(error.to_string()))?;
    let roots = load_backup_roots(&mut client, budget, config, name)?;

    let (target_lsn, target_head) = match target {
        RecoveryTarget::Backup => {
            let head = if roots.head.is_empty() {
                None
            } else {
                Some(
                    parse_commit_head(roots.head.readable())
                        .map_err(CheckpointSetupError::Corrupt)?
                        .1,
                )
            };
            (roots.floor, head)
        }
        RecoveryTarget::Lsn(lsn) => select_recovery_head(
            &mut client,
            budget,
            config,
            &roots,
            RecoveryTarget::Lsn(lsn),
        )?,
        RecoveryTarget::Time(time) => select_recovery_head(
            &mut client,
            budget,
            config,
            &roots,
            RecoveryTarget::Time(time),
        )?,
    };

    let pending = match target {
        RecoveryTarget::Backup => stack_format!(256, "{BACKUP_HEADER_V1}\nname {name}\nend\n"),
        _ => {
            let (first_lsn, digest) =
                target_head.map_or((0, 0), |head| (head.first_lsn, head.digest));
            stack_format!(
                256,
                "{POINT_IN_TIME_RESTORE_HEADER}\nname {name}\nlsn {target_lsn}\nhead {first_lsn} {digest:08x}\nmanifest_crc {:08x}\nend\n",
                roots.manifest_crc
            )
        }
    };
    match client.put(
        RESTORE_PENDING_KEY,
        pending.as_str().as_bytes(),
        Precondition::IfNoneMatchAny,
    ) {
        Ok(_) => {}
        Err(error) if error.is_precondition_failed() => {
            client.get(RESTORE_PENDING_KEY, None).map_err(|error| {
                CheckpointSetupError::ObjectStore(format!("load restore-pending: {error}"))
            })?;
            if client.body_bytes() != pending.as_str().as_bytes() {
                return Err(CheckpointSetupError::ObjectStore(
                    "another backup restore is pending".to_string(),
                ));
            }
        }
        Err(error) => {
            return Err(CheckpointSetupError::ObjectStore(format!(
                "publish restore-pending: {error}"
            )));
        }
    }

    let token = new_writer_token().map_err(|error| {
        CheckpointSetupError::ObjectStore(format!("create writer token: {error}"))
    })?;
    let mut fence_manifest = FixedBuf::new(
        budget,
        "restore writer-fence manifest",
        config.checkpoint_manifest_bytes,
    )
    .map_err(CheckpointSetupError::Budget)?;
    let mut fence_head = FixedBuf::new(budget, "restore writer-fence head", 128)
        .map_err(CheckpointSetupError::Budget)?;
    let mut fence = promote_writer(
        &mut client,
        token,
        &mut fence_manifest,
        &mut fence_head,
        PromotionRoots::Current,
    )?
    .fence;

    replace_root(
        &mut client,
        &mut fence,
        MANIFEST_KEY,
        roots.manifest.readable(),
    )?;
    match (target_head, target) {
        (None, _) => {
            replace_root(&mut client, &mut fence, COMMIT_HEAD_KEY, EMPTY_COMMIT_HEAD)?;
        }
        (Some(_), RecoveryTarget::Backup) => {
            replace_root(
                &mut client,
                &mut fence,
                COMMIT_HEAD_KEY,
                roots.head.readable(),
            )?;
        }
        (Some(target_head), _) => {
            let head = format_commit_head(token, target_head);
            replace_root(
                &mut client,
                &mut fence,
                COMMIT_HEAD_KEY,
                head.as_str().as_bytes(),
            )?;
        }
    }
    clear_local_cache(&config.data_dir)?;
    client
        .delete(RESTORE_PENDING_KEY)
        .map_err(|error| CheckpointSetupError::ObjectStore(format!("complete restore: {error}")))?;
    Ok(target_lsn)
}

fn select_recovery_head(
    client: &mut ObjectStore,
    budget: &mut Budget,
    config: &Config,
    roots: &BackupRoots,
    target: RecoveryTarget,
) -> Result<(u64, Option<CommitBatchId>), CheckpointSetupError> {
    if let RecoveryTarget::Lsn(lsn) = target
        && lsn < roots.floor
    {
        return Err(CheckpointSetupError::ObjectStore(format!(
            "recovery target LSN {lsn} precedes backup LSN {}",
            roots.floor
        )));
    }
    if let RecoveryTarget::Time(time) = target {
        let Some(created_at) = roots.created_at else {
            return Err(CheckpointSetupError::ObjectStore(
                "timestamp recovery requires a backup created with commit timestamps".to_string(),
            ));
        };
        if time < created_at {
            return Err(CheckpointSetupError::ObjectStore(
                "recovery target time precedes the named backup".to_string(),
            ));
        }
    }

    let backup_head = if roots.head.is_empty() {
        None
    } else {
        Some(
            parse_commit_head(roots.head.readable())
                .map_err(CheckpointSetupError::Corrupt)?
                .1,
        )
    };
    if matches!(target, RecoveryTarget::Lsn(lsn) if lsn == roots.floor) {
        return Ok((roots.floor, backup_head));
    }

    let live_head = match client.get(COMMIT_HEAD_KEY, None) {
        Ok(_) => {
            if is_empty_commit_head(client.body_bytes()).map_err(CheckpointSetupError::Corrupt)? {
                None
            } else {
                Some(
                    parse_commit_head(client.body_bytes())
                        .map_err(CheckpointSetupError::Corrupt)?
                        .1,
                )
            }
        }
        Err(error) if error.is_not_found() => None,
        Err(error) => {
            return Err(CheckpointSetupError::ObjectStore(format!(
                "load live commit-head for recovery: {error}"
            )));
        }
    };
    let mut chain = Vec::with_capacity(config.checkpoint_commit_batches);
    let mut current = live_head;
    while current != backup_head {
        let Some(id) = current else {
            return Err(CheckpointSetupError::ObjectStore(
                "live commit history does not descend from the named backup".to_string(),
            ));
        };
        if chain.len() == chain.capacity() {
            return Err(CheckpointSetupError::ObjectStore(format!(
                "commit-head chain exceeds checkpoint_commit_batches ({})",
                chain.capacity()
            )));
        }
        let key = stack_format!(72, "commits/{:020}-{:08x}.head", id.first_lsn, id.digest);
        client.get(key.as_str(), None).map_err(|error| {
            CheckpointSetupError::ObjectStore(format!("load recovery commit descriptor: {error}"))
        })?;
        let (described, previous) =
            parse_commit_descriptor(client.body_bytes()).map_err(CheckpointSetupError::Corrupt)?;
        if described != id {
            return Err(CheckpointSetupError::Corrupt(
                "commit descriptor does not match its head",
            ));
        }
        if id.first_lsn <= roots.floor {
            return Err(CheckpointSetupError::ObjectStore(
                "live commit history does not descend from the named backup".to_string(),
            ));
        }
        chain.push(CommitChainEntry { id, previous });
        current = previous;
    }

    let mut batch = FixedBuf::new(
        budget,
        "recovery commit batch",
        config.wal_buffer_bytes.max(config.wal_upload_buffer_bytes),
    )
    .map_err(CheckpointSetupError::Budget)?;
    let mut selected_head = backup_head;
    let mut selected_lsn = roots.floor;
    for entry in chain.iter().rev() {
        let key = stack_format!(
            72,
            "commits/{:020}-{:08x}.batch",
            entry.id.first_lsn,
            entry.id.digest
        );
        read_object_into(client, key.as_str(), &mut batch, "wal_buffer_bytes")?;
        if CommitBatchId::from_bytes(entry.id.first_lsn, batch.readable()) != entry.id {
            return Err(CheckpointSetupError::Corrupt(
                "commit batch does not match its digest",
            ));
        }
        let mut at = 0usize;
        let mut previous_lsn = 0u64;
        let mut last_boundary = 0usize;
        let mut selected_boundary_lsn = 0u64;
        let mut selected_boundary_end = 0usize;
        while at < batch.len() {
            let bytes = batch.readable();
            if bytes.len() - at < crate::wal::HEADER_LEN {
                return Err(CheckpointSetupError::Corrupt(
                    "commit batch ends inside a WAL header",
                ));
            }
            let payload_len =
                u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
            let total = crate::wal::HEADER_LEN
                .checked_add(payload_len)
                .filter(|total| at + total <= bytes.len())
                .ok_or(CheckpointSetupError::Corrupt(
                    "commit batch ends inside a WAL record",
                ))?;
            let lsn = u64::from_le_bytes(bytes[at + 8..at + 16].try_into().unwrap());
            if (at == 0 && lsn != entry.id.first_lsn)
                || (previous_lsn != 0 && lsn <= previous_lsn)
                || crate::wal::crc32c::crc32c(&bytes[at + 4..at + total])
                    != u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
            {
                return Err(CheckpointSetupError::Corrupt(
                    "commit batch has invalid WAL framing",
                ));
            }
            previous_lsn = lsn;
            let operation = crate::wal::decode_record(&bytes[at + 16..at + total]).ok_or(
                CheckpointSetupError::Corrupt("commit batch contains an unknown WAL record"),
            )?;
            let (boundary, boundary_time) = match operation {
                crate::wal::WalOp::Commit { committed_at, .. } => (true, committed_at),
                crate::wal::WalOp::PrepareTransaction { prepared_at, .. } => {
                    (true, Some(prepared_at))
                }
                _ => (false, None),
            };
            at += total;
            if !boundary {
                continue;
            }
            last_boundary = at;
            match target {
                RecoveryTarget::Lsn(wanted) if lsn == wanted => {
                    let head = select_batch_prefix(client, entry, &batch, at)?;
                    return Ok((lsn, Some(head)));
                }
                RecoveryTarget::Lsn(wanted) if lsn > wanted => {
                    return Err(CheckpointSetupError::ObjectStore(format!(
                        "recovery target LSN {wanted} is not a retained transaction boundary"
                    )));
                }
                RecoveryTarget::Time(wanted) => {
                    let boundary_time = boundary_time.ok_or_else(|| {
                        CheckpointSetupError::ObjectStore(format!(
                            "commit at LSN {lsn} has no timestamp; use an LSN target"
                        ))
                    })?;
                    if boundary_time > wanted {
                        if selected_boundary_end != 0 {
                            let head =
                                select_batch_prefix(client, entry, &batch, selected_boundary_end)?;
                            return Ok((selected_boundary_lsn, Some(head)));
                        }
                        return Ok((selected_lsn, selected_head));
                    }
                    selected_boundary_lsn = lsn;
                    selected_boundary_end = at;
                }
                _ => {}
            }
        }
        if last_boundary != batch.len() {
            return Err(CheckpointSetupError::Corrupt(
                "commit batch ends inside a transaction",
            ));
        }
        selected_head = Some(entry.id);
        selected_lsn = previous_lsn;
    }
    match target {
        RecoveryTarget::Lsn(wanted) => Err(CheckpointSetupError::ObjectStore(format!(
            "recovery target LSN {wanted} is newer than the retained commit history"
        ))),
        RecoveryTarget::Time(_) => Ok((selected_lsn, selected_head)),
        RecoveryTarget::Backup => unreachable!("backup target is resolved without history"),
    }
}

fn select_batch_prefix(
    client: &mut ObjectStore,
    entry: &CommitChainEntry,
    batch: &FixedBuf,
    end: usize,
) -> Result<CommitBatchId, CheckpointSetupError> {
    if end == batch.len() {
        return Ok(entry.id);
    }
    let bytes = &batch.readable()[..end];
    let selected = CommitBatchId::from_bytes(entry.id.first_lsn, bytes);
    let batch_key = stack_format!(
        72,
        "commits/{:020}-{:08x}.batch",
        selected.first_lsn,
        selected.digest
    );
    put_immutable_object(client, batch_key.as_str(), bytes)?;
    let descriptor_key = stack_format!(
        72,
        "commits/{:020}-{:08x}.head",
        selected.first_lsn,
        selected.digest
    );
    let previous = entry.previous.unwrap_or(CommitBatchId::EMPTY);
    let descriptor = stack_format!(
        96,
        "{}\nfirst {}\ndigest {:08x}\nprevious {} {:08x}\nend\n",
        COMMIT_HEAD_HEADER,
        selected.first_lsn,
        selected.digest,
        previous.first_lsn,
        previous.digest
    );
    put_immutable_object(
        client,
        descriptor_key.as_str(),
        descriptor.as_str().as_bytes(),
    )?;
    Ok(selected)
}

fn format_commit_head(writer: WriterToken, batch: CommitBatchId) -> StackStr<128> {
    stack_format!(
        128,
        "{}\nwriter {:032x}\nfirst {}\ndigest {:08x}\nend\n",
        COMMIT_HEAD_HEADER,
        writer.0,
        batch.first_lsn,
        batch.digest
    )
}

fn put_immutable_object(
    client: &mut ObjectStore,
    key: &str,
    bytes: &[u8],
) -> Result<(), CheckpointSetupError> {
    match client.put(key, bytes, Precondition::IfNoneMatchAny) {
        Ok(_) => Ok(()),
        Err(error) if error.is_precondition_failed() => {
            if object_matches(client, key, bytes)? {
                Ok(())
            } else {
                Err(CheckpointSetupError::ObjectStore(format!(
                    "immutable recovery object collision at {key}"
                )))
            }
        }
        Err(error) => Err(CheckpointSetupError::ObjectStore(format!(
            "write recovery object {key}: {error}"
        ))),
    }
}

/// Copies one named recovery point into an empty, independently configured
/// object-store prefix and publishes it as that prefix's live database. The
/// marker makes every partial copy restartable and blocks normal startup.
pub(crate) fn export_backup(
    source_config: &Config,
    destination_config: &Config,
    budget: &mut Budget,
    name: &str,
) -> Result<u64, CheckpointSetupError> {
    validate_backup_name(name)
        .map_err(|message| CheckpointSetupError::ObjectStore(message.to_string()))?;
    if !source_config.object_store_on || !destination_config.object_store_on {
        return Err(CheckpointSetupError::ObjectStore(
            "backup export requires object_store = on for source and destination".to_string(),
        ));
    }
    if data_dirs_match(&source_config.data_dir, &destination_config.data_dir) {
        return Err(CheckpointSetupError::ObjectStore(
            "backup export requires a distinct destination data_dir".to_string(),
        ));
    }

    let mut source = ObjectStore::new(source_config, budget)
        .map_err(|error| CheckpointSetupError::ObjectStore(error.to_string()))?;
    let mut destination = ObjectStore::new(destination_config, budget)
        .map_err(|error| CheckpointSetupError::ObjectStore(error.to_string()))?;
    let roots = load_backup_roots(&mut source, budget, source_config, name)?;
    if roots.manifest.len() > destination_config.checkpoint_manifest_bytes {
        return Err(CheckpointSetupError::ObjectStore(format!(
            "backup manifest exceeds destination checkpoint_manifest_bytes ({})",
            destination_config.checkpoint_manifest_bytes
        )));
    }
    let marker = stack_format!(
        192,
        "{EXPORT_HEADER}\nname {name}\nmanifest_crc {:08x}\nhead_crc {:08x}\nend\n",
        roots.manifest_crc,
        roots.head_crc
    );
    if begin_export(&mut destination, marker.as_str().as_bytes())? {
        if !export_roots_match(&mut destination, &roots)? {
            return Err(CheckpointSetupError::ObjectStore(
                "completed backup export destination has advanced from the exported roots"
                    .to_string(),
            ));
        }
        clear_local_cache(&destination_config.data_dir)?;
        destination.delete(EXPORT_PENDING_KEY).map_err(|error| {
            CheckpointSetupError::ObjectStore(format!("complete backup export: {error}"))
        })?;
        return Ok(roots.floor);
    }

    let copy_capacity = source_config
        .wal_buffer_bytes
        .max(source_config.wal_upload_buffer_bytes)
        .max(source_config.extension_script_bytes)
        .max(crate::store::BLOCK_SIZE)
        .max(source_config.checkpoint_manifest_bytes);
    let mut copy = FixedBuf::new(budget, "backup export object", copy_capacity)
        .map_err(CheckpointSetupError::Budget)?;
    let mut keys = Vec::with_capacity(source_config.checkpoint_garbage_batch_objects);
    for suffix in ["commit-head", "manifest", "complete"] {
        let key = stack_format!(128, "backups/{name}/{suffix}");
        copy_immutable_object(&mut source, &mut destination, key.as_str(), &mut copy)?;
    }
    copy_object_prefix(
        &mut source,
        &mut destination,
        "blocks/",
        &mut keys,
        &mut copy,
    )?;
    copy_object_prefix(
        &mut source,
        &mut destination,
        "commits/",
        &mut keys,
        &mut copy,
    )?;
    copy_object_prefix(
        &mut source,
        &mut destination,
        "extensions/meta/",
        &mut keys,
        &mut copy,
    )?;
    copy_object_prefix(
        &mut source,
        &mut destination,
        "extensions/sql/",
        &mut keys,
        &mut copy,
    )?;

    let token = new_writer_token().map_err(|error| {
        CheckpointSetupError::ObjectStore(format!("create destination writer token: {error}"))
    })?;
    let mut fence_manifest = FixedBuf::new(
        budget,
        "export writer-fence manifest",
        destination_config.checkpoint_manifest_bytes,
    )
    .map_err(CheckpointSetupError::Budget)?;
    let mut fence_head = FixedBuf::new(budget, "export writer-fence head", 128)
        .map_err(CheckpointSetupError::Budget)?;
    let mut fence = promote_writer(
        &mut destination,
        token,
        &mut fence_manifest,
        &mut fence_head,
        PromotionRoots::Current,
    )?
    .fence;

    replace_root(
        &mut destination,
        &mut fence,
        MANIFEST_KEY,
        roots.manifest.readable(),
    )?;
    if roots.head.is_empty() {
        replace_root(
            &mut destination,
            &mut fence,
            COMMIT_HEAD_KEY,
            EMPTY_COMMIT_HEAD,
        )?;
    } else {
        replace_root(
            &mut destination,
            &mut fence,
            COMMIT_HEAD_KEY,
            roots.head.readable(),
        )?;
    }
    clear_local_cache(&destination_config.data_dir)?;
    publish_export_complete(&mut destination, marker.as_str().as_bytes())?;
    destination.delete(EXPORT_PENDING_KEY).map_err(|error| {
        CheckpointSetupError::ObjectStore(format!("complete backup export: {error}"))
    })?;
    Ok(roots.floor)
}

fn data_dirs_match(source: &str, destination: &str) -> bool {
    if std::path::Path::new(source) == std::path::Path::new(destination) {
        return true;
    }
    match (
        std::fs::canonicalize(source),
        std::fs::canonicalize(destination),
    ) {
        (Ok(source), Ok(destination)) => source == destination,
        _ => false,
    }
}

fn begin_export(
    destination: &mut ObjectStore,
    marker: &[u8],
) -> Result<bool, CheckpointSetupError> {
    let complete = matching_marker(destination, EXPORT_COMPLETE_KEY, marker)?;
    if complete == Some(false) {
        return Err(CheckpointSetupError::ObjectStore(
            "another backup export completed in the destination".to_string(),
        ));
    }
    match matching_marker(destination, EXPORT_PENDING_KEY, marker)? {
        Some(matches) => {
            if !matches {
                return Err(CheckpointSetupError::ObjectStore(
                    "another backup export is pending in the destination".to_string(),
                ));
            }
            return Ok(complete == Some(true));
        }
        None if complete == Some(true) => return Ok(true),
        None => {}
    }
    let mut occupied = false;
    destination
        .list_batch("", "", 1, |_| occupied = true)
        .map_err(|error| {
            CheckpointSetupError::ObjectStore(format!("inspect export destination: {error}"))
        })?;
    if occupied {
        return Err(CheckpointSetupError::ObjectStore(
            "backup export destination prefix is not empty".to_string(),
        ));
    }
    destination
        .put(EXPORT_PENDING_KEY, marker, Precondition::IfNoneMatchAny)
        .map_err(|error| {
            CheckpointSetupError::ObjectStore(format!("publish export-pending: {error}"))
        })?;
    Ok(false)
}

fn matching_marker(
    client: &mut ObjectStore,
    key: &str,
    expected: &[u8],
) -> Result<Option<bool>, CheckpointSetupError> {
    match client.get(
        key,
        Some(ByteRange::new(0, 0).expect("one-byte marker probe")),
    ) {
        Ok(_) => object_matches(client, key, expected).map(Some),
        Err(ObjectError::Status { code: 416, .. }) => Ok(Some(false)),
        Err(error) if error.is_not_found() => Ok(None),
        Err(error) => Err(CheckpointSetupError::ObjectStore(format!(
            "load destination {key}: {error}"
        ))),
    }
}

fn export_roots_match(
    destination: &mut ObjectStore,
    roots: &BackupRoots,
) -> Result<bool, CheckpointSetupError> {
    if !object_matches(destination, MANIFEST_KEY, roots.manifest.readable())? {
        return Ok(false);
    }
    if !roots.head.is_empty() {
        return object_matches(destination, COMMIT_HEAD_KEY, roots.head.readable());
    }
    match destination.get(
        COMMIT_HEAD_KEY,
        Some(ByteRange::new(0, 0).expect("one-byte root probe")),
    ) {
        Err(error) if error.is_not_found() => Ok(true),
        Ok(_) => {
            destination.get(COMMIT_HEAD_KEY, None).map_err(|error| {
                CheckpointSetupError::ObjectStore(format!(
                    "verify backup export commit-head: {error}"
                ))
            })?;
            is_empty_commit_head(destination.body_bytes()).map_err(CheckpointSetupError::Corrupt)
        }
        Err(ObjectError::Status { code: 416, .. }) => Ok(false),
        Err(error) => Err(CheckpointSetupError::ObjectStore(format!(
            "verify backup export commit-head: {error}"
        ))),
    }
}

fn publish_export_complete(
    destination: &mut ObjectStore,
    marker: &[u8],
) -> Result<(), CheckpointSetupError> {
    match destination.put(EXPORT_COMPLETE_KEY, marker, Precondition::IfNoneMatchAny) {
        Ok(_) => Ok(()),
        Err(error) if error.is_precondition_failed() => {
            if object_matches(destination, EXPORT_COMPLETE_KEY, marker)? {
                Ok(())
            } else {
                Err(CheckpointSetupError::ObjectStore(
                    "another backup export completed in the destination".to_string(),
                ))
            }
        }
        Err(error) => Err(CheckpointSetupError::ObjectStore(format!(
            "publish export-complete: {error}"
        ))),
    }
}

fn copy_object_prefix(
    source: &mut ObjectStore,
    destination: &mut ObjectStore,
    prefix: &str,
    keys: &mut Vec<StackStr<{ crate::object_store::MAX_OBJECT_KEY_BYTES }>>,
    copy: &mut FixedBuf,
) -> Result<(), CheckpointSetupError> {
    let mut after = StackStr::<{ crate::object_store::MAX_OBJECT_KEY_BYTES }>::new();
    loop {
        keys.clear();
        source
            .list_batch(prefix, after.as_str(), keys.capacity(), |key| {
                keys.push(StackStr::from_str(key));
            })
            .map_err(|error| {
                CheckpointSetupError::ObjectStore(format!("list backup export {prefix}: {error}"))
            })?;
        if keys.is_empty() {
            break;
        }
        for key in keys.iter() {
            if key.is_truncated() {
                return Err(CheckpointSetupError::ObjectStore(
                    "backup export object key exceeds the S3 key limit".to_string(),
                ));
            }
            copy_immutable_object(source, destination, key.as_str(), copy)?;
        }
        after = *keys.last().expect("nonempty export batch");
    }
    Ok(())
}

fn copy_immutable_object(
    source: &mut ObjectStore,
    destination: &mut ObjectStore,
    key: &str,
    copy: &mut FixedBuf,
) -> Result<(), CheckpointSetupError> {
    read_object_into(source, key, copy, "backup export object buffer")?;
    match destination.put(key, copy.readable(), Precondition::IfNoneMatchAny) {
        Ok(_) => Ok(()),
        Err(error) if error.is_precondition_failed() => {
            if object_matches(destination, key, copy.readable())? {
                Ok(())
            } else {
                Err(CheckpointSetupError::ObjectStore(format!(
                    "immutable backup export collision at {key}"
                )))
            }
        }
        Err(error) => Err(CheckpointSetupError::ObjectStore(format!(
            "write backup export object {key}: {error}"
        ))),
    }
}

fn read_object_into(
    client: &mut ObjectStore,
    key: &str,
    out: &mut FixedBuf,
    limit: &'static str,
) -> Result<(), CheckpointSetupError> {
    out.clear();
    let window = client.response_capacity();
    if window == 0 {
        return Err(CheckpointSetupError::ObjectStore(
            "object_store_response_bytes must be greater than zero".to_string(),
        ));
    }
    let mut offset = 0u64;
    loop {
        let last = offset.checked_add(window as u64 - 1).ok_or_else(|| {
            CheckpointSetupError::ObjectStore(format!("object {key} is too large"))
        })?;
        match client.get(
            key,
            Some(ByteRange::new(offset, last).expect("nonempty export range")),
        ) {
            Ok(_) => {}
            Err(ObjectError::Status { code: 416, .. }) => break,
            Err(error) => {
                return Err(CheckpointSetupError::ObjectStore(format!(
                    "read object {key}: {error}"
                )));
            }
        }
        let bytes = client.body_bytes();
        if bytes.is_empty() {
            return Err(CheckpointSetupError::ObjectStore(format!(
                "read object {key}: empty successful range response"
            )));
        }
        if !out.append(bytes) {
            return Err(CheckpointSetupError::ObjectStore(format!(
                "object {key} exceeds {limit} ({})",
                out.capacity()
            )));
        }
        offset += bytes.len() as u64;
        if bytes.len() < window {
            break;
        }
    }
    Ok(())
}

fn object_matches(
    client: &mut ObjectStore,
    key: &str,
    expected: &[u8],
) -> Result<bool, CheckpointSetupError> {
    if client.response_capacity() == 0 {
        return Err(CheckpointSetupError::ObjectStore(
            "object_store_response_bytes must be greater than zero".to_string(),
        ));
    }
    let mut offset = 0usize;
    while offset < expected.len() {
        let last = offset
            .saturating_add(client.response_capacity())
            .min(expected.len())
            - 1;
        let result = client
            .get(
                key,
                Some(ByteRange::new(offset as u64, last as u64).expect("ordered compare range")),
            )
            .map_err(|error| {
                CheckpointSetupError::ObjectStore(format!(
                    "verify backup export object {key}: {error}"
                ))
            })?;
        if result.len != last + 1 - offset || client.body_bytes() != &expected[offset..=last] {
            return Ok(false);
        }
        offset = last + 1;
    }
    match client.get(
        key,
        Some(ByteRange::new(offset as u64, offset as u64).expect("one-byte compare range")),
    ) {
        Err(ObjectError::Status { code: 416, .. }) => Ok(true),
        Ok(_) => Ok(false),
        Err(error) => Err(CheckpointSetupError::ObjectStore(format!(
            "verify backup export object length {key}: {error}"
        ))),
    }
}

fn promote_writer(
    client: &mut ObjectStore,
    token: WriterToken,
    manifest: &mut FixedBuf,
    head: &mut FixedBuf,
    roots: PromotionRoots,
) -> Result<WriterPromotion, CheckpointSetupError> {
    // Reject incompatible or malformed root records before displacing a writer.
    // Publication re-reads them after fencing so intervening commits are preserved.
    let manifest_etag = read_mutable_root(client, MutableRoot::Manifest, manifest)?;
    roots.check(MutableRoot::Manifest, manifest_etag)?;
    retag_manifest_root(manifest, token)?;
    let head_etag = read_mutable_root(client, MutableRoot::CommitHead, head)?;
    roots.check(MutableRoot::CommitHead, head_etag)?;
    retag_commit_head_root(head, token)?;

    let current = match client.get(WRITER_FENCE_KEY, None) {
        Ok(result) => {
            parse_writer_fence(client.body_bytes()).map_err(CheckpointSetupError::Corrupt)?;
            Some(result.etag)
        }
        Err(error) if error.is_not_found() => None,
        Err(error) => {
            return Err(CheckpointSetupError::ObjectStore(format!(
                "load writer fence: {error}"
            )));
        }
    };
    let transition = writer_fence_record("transition", token);
    let condition = current
        .as_ref()
        .map_or(Precondition::IfNoneMatchAny, Precondition::IfMatch);
    let transition_etag = put_fence_record(client, transition.as_str().as_bytes(), condition)?;

    let manifest_etag = fence_mutable_root(client, MutableRoot::Manifest, manifest, token, roots)?;
    let head_etag = fence_mutable_root(client, MutableRoot::CommitHead, head, token, roots)?;

    let active = writer_fence_record("active", token);
    let etag = put_fence_record(
        client,
        active.as_str().as_bytes(),
        Precondition::IfMatch(&transition_etag),
    )?;
    Ok(WriterPromotion {
        fence: WriterFence { token, etag },
        manifest: manifest_etag,
        head: head_etag,
    })
}

fn put_fence_record(
    client: &mut ObjectStore,
    body: &[u8],
    condition: Precondition<'_>,
) -> Result<EntityTag, CheckpointSetupError> {
    match client.put(WRITER_FENCE_KEY, body, condition) {
        Ok(etag) => Ok(etag),
        Err(error) => match client.get(WRITER_FENCE_KEY, None) {
            Ok(result) if client.body_bytes() == body => Ok(result.etag),
            Ok(_) | Err(ObjectError::Status { code: 404, .. }) => {
                Err(CheckpointSetupError::ObjectStore(
                    "writer ownership changed during promotion".to_string(),
                ))
            }
            Err(refresh) => Err(CheckpointSetupError::ObjectStore(format!(
                "promote writer: {error}; verify writer fence: {refresh}"
            ))),
        },
    }
}

fn assert_writer_fence(
    client: &mut ObjectStore,
    fence: &mut WriterFence,
) -> Result<(), CheckpointSetupError> {
    let active = writer_fence_record("active", fence.token);
    match client.put(
        WRITER_FENCE_KEY,
        active.as_str().as_bytes(),
        Precondition::IfMatch(&fence.etag),
    ) {
        Ok(etag) => {
            fence.etag = etag;
            Ok(())
        }
        Err(error) => match client.get(WRITER_FENCE_KEY, None) {
            Ok(result) if client.body_bytes() == active.as_str().as_bytes() => {
                fence.etag = result.etag;
                Ok(())
            }
            Ok(_) | Err(ObjectError::Status { code: 404, .. }) => {
                Err(CheckpointSetupError::ObjectStore(
                    "writer was fenced by a newer process".to_string(),
                ))
            }
            Err(refresh) => Err(CheckpointSetupError::ObjectStore(format!(
                "verify writer ownership: {error}; refresh writer fence: {refresh}"
            ))),
        },
    }
}

#[derive(Clone, Copy)]
enum MutableRoot {
    Manifest,
    CommitHead,
}

impl MutableRoot {
    fn key(self) -> &'static str {
        match self {
            Self::Manifest => MANIFEST_KEY,
            Self::CommitHead => COMMIT_HEAD_KEY,
        }
    }
}

fn root_writer(root: MutableRoot, bytes: &[u8]) -> Option<WriterToken> {
    match root {
        MutableRoot::Manifest => {
            if bytes.starts_with(EMPTY_MANIFEST_HEADER.as_bytes()) {
                empty_manifest_writer(bytes).ok().flatten()
            } else {
                core::str::from_utf8(bytes)
                    .ok()
                    .and_then(|text| parse_manifest_metadata(text).ok())
                    .map(|metadata| metadata.writer)
            }
        }
        MutableRoot::CommitHead => {
            if bytes.starts_with(EMPTY_COMMIT_HEAD_HEADER.as_bytes()) {
                empty_commit_head_writer(bytes).ok().flatten()
            } else {
                parse_commit_head(bytes).ok().map(|(writer, _)| writer)
            }
        }
    }
}

fn retag_manifest_root(
    manifest: &mut FixedBuf,
    token: WriterToken,
) -> Result<(), CheckpointSetupError> {
    if manifest.is_empty()
        || is_empty_manifest(manifest.readable()).map_err(CheckpointSetupError::Corrupt)?
    {
        let empty = empty_manifest_record(token);
        manifest.clear();
        if !manifest.append(empty.as_str().as_bytes()) {
            return Err(CheckpointSetupError::Corrupt(
                "empty manifest exceeds its root buffer",
            ));
        }
        return Ok(());
    }

    let text = core::str::from_utf8(manifest.readable())
        .map_err(|_| CheckpointSetupError::Corrupt("manifest is not UTF-8"))?;
    let metadata = parse_manifest_metadata(text).map_err(CheckpointSetupError::Corrupt)?;
    let (start, end) = metadata.writer_range;
    let writer = stack_format!(48, "writer {:032x}", token.0);
    manifest.filled_mut()[start..end].copy_from_slice(writer.as_str().as_bytes());
    Ok(())
}

fn retag_commit_head_root(
    head: &mut FixedBuf,
    token: WriterToken,
) -> Result<(), CheckpointSetupError> {
    let replacement = if head.is_empty()
        || is_empty_commit_head(head.readable()).map_err(CheckpointSetupError::Corrupt)?
    {
        empty_commit_head_record(token)
    } else {
        let (_, batch) =
            parse_commit_head(head.readable()).map_err(CheckpointSetupError::Corrupt)?;
        format_commit_head(token, batch)
    };
    head.clear();
    if !head.append(replacement.as_str().as_bytes()) {
        return Err(CheckpointSetupError::Corrupt(
            "commit-head exceeds its root buffer",
        ));
    }
    Ok(())
}

fn fence_mutable_root(
    client: &mut ObjectStore,
    root: MutableRoot,
    scratch: &mut FixedBuf,
    token: WriterToken,
    roots: PromotionRoots,
) -> Result<EntityTag, CheckpointSetupError> {
    let key = root.key();
    for _ in 0..16 {
        let current = read_mutable_root(client, root, scratch)?;
        roots.check(root, current)?;
        match root {
            MutableRoot::Manifest => retag_manifest_root(scratch, token)?,
            MutableRoot::CommitHead => retag_commit_head_root(scratch, token)?,
        }
        let condition = current
            .as_ref()
            .map_or(Precondition::IfNoneMatchAny, Precondition::IfMatch);
        match client.put(key, scratch.readable(), condition) {
            Ok(etag) => return Ok(etag),
            Err(error) => {
                let refreshed = read_mutable_root(client, root, scratch)?;
                if let Some(etag) = refreshed
                    && root_writer(root, scratch.readable()) == Some(token)
                {
                    return Ok(etag);
                }
                if !error.is_precondition_failed()
                    || current
                        .as_ref()
                        .is_some_and(|expected| refreshed == Some(*expected))
                {
                    return Err(CheckpointSetupError::ObjectStore(format!(
                        "fence {key}: {error}"
                    )));
                }
            }
        }
    }
    Err(CheckpointSetupError::ObjectStore(format!(
        "{key} kept changing during writer promotion"
    )))
}

fn read_mutable_root(
    client: &mut ObjectStore,
    root: MutableRoot,
    out: &mut FixedBuf,
) -> Result<Option<EntityTag>, CheckpointSetupError> {
    let key = root.key();
    out.clear();
    let window = client.response_capacity();
    if window == 0 {
        return Err(CheckpointSetupError::ObjectStore(
            "object_store_response_bytes must be greater than zero".to_string(),
        ));
    }
    let mut offset = 0u64;
    let mut etag = None;
    let mut restarts = 0u8;
    loop {
        let last = offset
            .checked_add(window as u64 - 1)
            .ok_or_else(|| CheckpointSetupError::ObjectStore(format!("{key} is too large")))?;
        match client.get(
            key,
            Some(ByteRange::new(offset, last).expect("nonempty mutable-root range")),
        ) {
            Ok(result) => {
                if etag.is_some_and(|first| first != result.etag) {
                    restarts += 1;
                    if restarts == 16 {
                        return Err(CheckpointSetupError::ObjectStore(format!(
                            "{key} kept changing while it was read for writer promotion"
                        )));
                    }
                    out.clear();
                    offset = 0;
                    etag = None;
                    continue;
                }
                etag = Some(result.etag);
            }
            Err(ObjectError::Status { code: 416, .. }) if etag.is_some() => break,
            Err(ObjectError::Status { code: 416, .. }) => {
                return Err(CheckpointSetupError::Corrupt(match root {
                    MutableRoot::Manifest => "manifest root is empty",
                    MutableRoot::CommitHead => "commit-head root is empty",
                }));
            }
            Err(error) if error.is_not_found() && etag.is_none() => return Ok(None),
            Err(error) => {
                return Err(CheckpointSetupError::ObjectStore(format!(
                    "read {key} for writer promotion: {error}"
                )));
            }
        }
        let bytes = client.body_bytes();
        if bytes.is_empty() {
            return Err(CheckpointSetupError::ObjectStore(format!(
                "read {key} for writer promotion: empty successful range response"
            )));
        }
        if !out.append(bytes) {
            return Err(CheckpointSetupError::ObjectStore(format!(
                "{key} exceeds its configured root buffer ({})",
                out.capacity()
            )));
        }
        offset += bytes.len() as u64;
        if bytes.len() < window {
            break;
        }
    }
    Ok(etag)
}

fn replace_root(
    client: &mut ObjectStore,
    fence: &mut WriterFence,
    key: &str,
    bytes: &[u8],
) -> Result<(), CheckpointSetupError> {
    assert_writer_fence(client, fence)?;
    let current = match client.get(key, None) {
        Ok(result) => Some(result.etag),
        Err(error) if error.is_not_found() => None,
        Err(error) => {
            return Err(CheckpointSetupError::ObjectStore(format!(
                "load {key} for restore: {error}"
            )));
        }
    };
    let condition = current
        .as_ref()
        .map_or(Precondition::IfNoneMatchAny, Precondition::IfMatch);
    client.put(key, bytes, condition).map_err(|error| {
        let detail = if error.is_precondition_failed() {
            "writer ownership changed during durable root replacement; retry".to_string()
        } else {
            format!("replace {key}: {error}")
        };
        CheckpointSetupError::ObjectStore(detail)
    })?;
    Ok(())
}

fn clear_local_cache(data_dir: &str) -> Result<(), CheckpointSetupError> {
    let directory = std::path::Path::new(data_dir);
    std::fs::create_dir_all(directory).map_err(|error| {
        CheckpointSetupError::ObjectStore(format!("create data directory for restore: {error}"))
    })?;
    for name in ["journal.wal", "block-cache", "clean.shutdown"] {
        let path = directory.join(name);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(CheckpointSetupError::ObjectStore(format!(
                    "remove local cache '{}': {error}",
                    path.display()
                )));
            }
        }
    }
    std::fs::File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|error| {
            CheckpointSetupError::ObjectStore(format!(
                "sync restored data directory '{}': {error}",
                directory.display()
            ))
        })
}

fn parse_backup_complete(bytes: &[u8]) -> Result<(u64, Option<i64>, u32, u32), &'static str> {
    let text = core::str::from_utf8(bytes).map_err(|_| "backup completion is not UTF-8")?;
    let mut lines = text.lines();
    let version = match lines.next() {
        Some(header) if header == BACKUP_HEADER_V1 => 1,
        Some(header) if header == BACKUP_HEADER => 2,
        _ => return Err("bad backup completion header"),
    };
    let floor = lines
        .next()
        .and_then(|line| line.strip_prefix("lsn "))
        .and_then(|value| value.parse().ok())
        .ok_or("bad backup completion LSN")?;
    let created_at = if version == 2 {
        Some(
            lines
                .next()
                .and_then(|line| line.strip_prefix("created_at "))
                .and_then(|value| value.parse().ok())
                .ok_or("bad backup creation timestamp")?,
        )
    } else {
        None
    };
    let manifest_crc = lines
        .next()
        .and_then(|line| line.strip_prefix("manifest_crc "))
        .and_then(|value| u32::from_str_radix(value, 16).ok())
        .ok_or("bad backup manifest checksum")?;
    let head_crc = lines
        .next()
        .and_then(|line| line.strip_prefix("head_crc "))
        .and_then(|value| u32::from_str_radix(value, 16).ok())
        .ok_or("bad backup commit-head checksum")?;
    if lines.next() != Some("end") || lines.next().is_some() {
        return Err("bad backup completion terminator");
    }
    Ok((floor, created_at, manifest_crc, head_crc))
}

fn validate_backup_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() || name.len() > BACKUP_NAME_BYTES {
        return Err("backup name must contain between 1 and 63 bytes");
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        || matches!(name, "." | "..")
    {
        return Err("backup name may contain only ASCII letters, digits, '.', '_', and '-'");
    }
    Ok(())
}

fn parse_commit_head(bytes: &[u8]) -> Result<(WriterToken, CommitBatchId), &'static str> {
    let text = core::str::from_utf8(bytes).map_err(|_| "commit-head is not UTF-8")?;
    let mut lines = text.lines();
    if lines.next() != Some(COMMIT_HEAD_HEADER) {
        return Err("bad commit-head header");
    }
    let writer = lines
        .next()
        .and_then(|line| line.strip_prefix("writer "))
        .and_then(|word| u128::from_str_radix(word, 16).ok())
        .filter(|writer| *writer != 0)
        .map(WriterToken)
        .ok_or("bad commit-head writer")?;
    let first_lsn = lines
        .next()
        .and_then(|line| line.strip_prefix("first "))
        .and_then(|word| word.parse().ok())
        .filter(|first_lsn| *first_lsn != 0)
        .ok_or("bad commit-head first LSN")?;
    let digest = lines
        .next()
        .and_then(|line| line.strip_prefix("digest "))
        .and_then(|word| u32::from_str_radix(word, 16).ok())
        .ok_or("bad commit-head digest")?;
    if lines.next() != Some("end") || lines.next().is_some() {
        return Err("bad commit-head terminator");
    }
    Ok((writer, CommitBatchId { first_lsn, digest }))
}

fn parse_commit_descriptor(
    bytes: &[u8],
) -> Result<(CommitBatchId, Option<CommitBatchId>), &'static str> {
    let text = core::str::from_utf8(bytes).map_err(|_| "commit descriptor is not UTF-8")?;
    let mut lines = text.lines();
    if lines.next() != Some(COMMIT_HEAD_HEADER) {
        return Err("bad commit descriptor header");
    }
    let first_lsn: u64 = lines
        .next()
        .and_then(|line| line.strip_prefix("first "))
        .and_then(|word| word.parse().ok())
        .filter(|first_lsn| *first_lsn != 0)
        .ok_or("bad commit descriptor first LSN")?;
    let digest = lines
        .next()
        .and_then(|line| line.strip_prefix("digest "))
        .and_then(|word| u32::from_str_radix(word, 16).ok())
        .ok_or("bad commit descriptor digest")?;
    let (previous_lsn, previous_digest) = lines
        .next()
        .and_then(|line| line.strip_prefix("previous "))
        .and_then(|word| {
            let mut words = word.split(' ');
            Some((
                words.next()?.parse().ok()?,
                u32::from_str_radix(words.next()?, 16).ok()?,
            ))
        })
        .ok_or("bad commit descriptor previous LSN")?;
    if (previous_lsn != 0 && previous_lsn >= first_lsn)
        || (previous_lsn == 0 && previous_digest != 0)
        || lines.next() != Some("end")
        || lines.next().is_some()
    {
        return Err("bad commit descriptor chain");
    }
    let previous = (previous_lsn != 0).then_some(CommitBatchId {
        first_lsn: previous_lsn,
        digest: previous_digest,
    });
    Ok((CommitBatchId { first_lsn, digest }, previous))
}

fn parse_field<T: core::str::FromStr>(
    word: Option<&str>,
    what: &'static str,
) -> Result<T, CheckpointSetupError> {
    word.and_then(|w| w.parse().ok())
        .ok_or(CheckpointSetupError::Corrupt(what))
}

fn parse_foreign_options(
    words: &mut core::str::Split<'_, char>,
    count: usize,
) -> Result<crate::storage::foreign::ForeignOptions, CheckpointSetupError> {
    if count > crate::storage::foreign::MAX_FOREIGN_OPTIONS {
        return Err(CheckpointSetupError::Corrupt("too many foreign options"));
    }
    let mut options = crate::storage::foreign::ForeignOptions::EMPTY;
    for _ in 0..count {
        let name = decode_hex_name(
            words
                .next()
                .ok_or(CheckpointSetupError::Corrupt("foreign option name missing"))?,
        )?;
        let value = decode_hex_name(words.next().ok_or(CheckpointSetupError::Corrupt(
            "foreign option value missing",
        ))?)?;
        options
            .restore_option(&name, &value)
            .map_err(|_| CheckpointSetupError::Corrupt("invalid foreign option"))?;
    }
    Ok(options)
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
    String::from_utf8(decode_manifest_hex_bytes(hex)?)
        .map_err(|_| CheckpointSetupError::Corrupt("hex name not UTF-8"))
}

fn decode_manifest_hex_bytes(hex: &str) -> Result<Vec<u8>, CheckpointSetupError> {
    if hex == "-" {
        return Ok(Vec::new());
    }
    if !hex.len().is_multiple_of(2) {
        return Err(CheckpointSetupError::Corrupt("odd-length hex bytes"));
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for pair in hex.as_bytes().as_chunks::<2>().0 {
        let digit = |byte: u8| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        };
        let high = digit(pair[0]).ok_or(CheckpointSetupError::Corrupt("bad hex bytes"))?;
        let low = digit(pair[1]).ok_or(CheckpointSetupError::Corrupt("bad hex bytes"))?;
        bytes.push((high << 4) | low);
    }
    Ok(bytes)
}

fn decode_optional_hex_name(value: &str) -> Result<Option<String>, CheckpointSetupError> {
    match value.as_bytes().first() {
        Some(b'0') if value.len() == 1 => Ok(None),
        Some(b'1') => decode_hex_name(&value[1..]).map(Some),
        _ => Err(CheckpointSetupError::Corrupt(
            "invalid optional foreign value",
        )),
    }
}

fn manifest_hex<const N: usize>(value: &str) -> StackStr<N> {
    use core::fmt::Write as _;
    let mut encoded = StackStr::new();
    if value.is_empty() {
        let _ = encoded.write_char('-');
    } else {
        for byte in value.as_bytes() {
            let _ = write!(encoded, "{byte:02x}");
        }
    }
    encoded
}

fn manifest_optional_hex(
    value: Option<StackStr<{ crate::storage::foreign::FOREIGN_OPTION_VALUE_MAX }>>,
) -> StackStr<{ 2 * crate::storage::foreign::FOREIGN_OPTION_VALUE_MAX + 2 }> {
    use core::fmt::Write as _;
    let mut encoded = StackStr::new();
    match value {
        None => {
            let _ = encoded.write_char('0');
        }
        Some(value) => {
            let _ = encoded.write_char('1');
            let _ = encoded.write_str(
                manifest_hex::<{ 2 * crate::storage::foreign::FOREIGN_OPTION_VALUE_MAX + 1 }>(
                    value.as_str(),
                )
                .as_str(),
            );
        }
    }
    encoded
}

fn encode_extension_package(
    package: crate::storage::ExtensionPackage,
) -> Result<StackStr<2048>, SqlError> {
    use core::fmt::Write as _;
    fn hex<const N: usize>(value: &str) -> StackStr<N> {
        use core::fmt::Write as _;
        let mut encoded = StackStr::new();
        for byte in value.as_bytes() {
            let _ = write!(encoded, "{byte:02x}");
        }
        if value.is_empty() {
            let _ = encoded.write_str("-");
        }
        encoded
    }
    let name = hex::<130>(package.name.as_str());
    let default = package.default_version.map_or_else(
        || StackStr::from_str("-"),
        |value| hex::<130>(value.as_str()),
    );
    let schema = package.schema.map_or_else(
        || StackStr::from_str("-"),
        |value| hex::<130>(value.as_str()),
    );
    let comment = hex::<{ crate::storage::COMMENT_MAX * 2 + 2 }>(package.comment.as_str());
    let mut encoded = StackStr::<2048>::new();
    let code = match package.code {
        crate::storage::ExtensionPackageCode::Sql => 0,
        crate::storage::ExtensionPackageCode::NativeLibrary => 1,
    };
    write!(
        encoded,
        "{} {} {} {} {} {} {} {} {} {}",
        EXTENSION_PACKAGE_HEADER,
        name.as_str(),
        default.as_str(),
        schema.as_str(),
        u8::from(package.relocatable),
        u8::from(package.superuser),
        u8::from(package.trusted),
        code,
        comment.as_str(),
        package.requires().len(),
    )
    .map_err(|_| {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "extension package metadata is too long"
        )
    })?;
    for required in package.requires() {
        let required = hex::<130>(required.as_str());
        write!(encoded, " {}", required.as_str()).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "extension package metadata is too long"
            )
        })?;
    }
    write!(encoded, " {}", package.no_relocate().len()).map_err(|_| {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "extension package metadata is too long"
        )
    })?;
    for required in package.no_relocate() {
        let required = hex::<130>(required.as_str());
        write!(encoded, " {}", required.as_str()).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "extension package metadata is too long"
            )
        })?;
    }
    if encoded.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "extension package metadata is too long"
        ));
    }
    Ok(encoded)
}

fn decode_extension_package(
    bytes: &[u8],
) -> Result<crate::storage::ExtensionPackage, CheckpointSetupError> {
    let text = core::str::from_utf8(bytes)
        .map_err(|_| CheckpointSetupError::Corrupt("extension package metadata is not UTF-8"))?;
    let mut words = text.split(' ');
    if words.next() != Some(EXTENSION_PACKAGE_HEADER) {
        return Err(CheckpointSetupError::Corrupt(
            "bad extension package header",
        ));
    }
    let name = sql_name(&decode_hex_name(next_manifest_word(
        &mut words,
        "extension package name",
    )?)?)?;
    let default = decode_hex_name(next_manifest_word(&mut words, "extension default version")?)?;
    let schema = decode_hex_name(next_manifest_word(&mut words, "extension package schema")?)?;
    let relocatable = parse_bool_field(words.next(), "extension relocatable")?;
    let superuser = parse_bool_field(words.next(), "extension superuser")?;
    let trusted = parse_bool_field(words.next(), "extension trusted")?;
    let code = match parse_field::<u8>(words.next(), "extension package code")? {
        0 => crate::storage::ExtensionPackageCode::Sql,
        1 => crate::storage::ExtensionPackageCode::NativeLibrary,
        _ => {
            return Err(CheckpointSetupError::Corrupt(
                "invalid extension package code",
            ));
        }
    };
    let comment = decode_hex_name(next_manifest_word(&mut words, "extension package comment")?)?;
    let comment = crate::storage::comment_stackstr(&comment)
        .map_err(|_| CheckpointSetupError::Corrupt("extension package comment is too long"))?;
    let require_count: usize = parse_field(words.next(), "extension requirement count")?;
    if require_count > crate::storage::MAX_EXTENSION_REQUIRES {
        return Err(CheckpointSetupError::Corrupt(
            "too many extension requirements",
        ));
    }
    let mut requires = [SqlName::EMPTY; crate::storage::MAX_EXTENSION_REQUIRES];
    for required in &mut requires[..require_count] {
        *required = sql_name(&decode_hex_name(next_manifest_word(
            &mut words,
            "extension requirement",
        )?)?)?;
    }
    let no_relocate_count: usize = parse_field(words.next(), "extension no_relocate count")?;
    if no_relocate_count > crate::storage::MAX_EXTENSION_REQUIRES {
        return Err(CheckpointSetupError::Corrupt(
            "too many extension no_relocate entries",
        ));
    }
    let mut no_relocate = [SqlName::EMPTY; crate::storage::MAX_EXTENSION_REQUIRES];
    for required in &mut no_relocate[..no_relocate_count] {
        *required = sql_name(&decode_hex_name(next_manifest_word(
            &mut words,
            "extension no_relocate entry",
        )?)?)?;
    }
    if words.next().is_some()
        || relocatable && !schema.is_empty()
        || no_relocate[..no_relocate_count]
            .iter()
            .any(|required| !requires[..require_count].contains(required))
    {
        return Err(CheckpointSetupError::Corrupt(
            "invalid extension package metadata",
        ));
    }
    Ok(crate::storage::ExtensionPackage {
        name,
        default_version: if default.is_empty() {
            None
        } else {
            Some(
                crate::storage::ExtensionVersion::parse(&default).map_err(|_| {
                    CheckpointSetupError::Corrupt("invalid extension default version")
                })?,
            )
        },
        schema: (!schema.is_empty())
            .then(|| sql_name(&schema))
            .transpose()?,
        relocatable,
        superuser,
        trusted,
        code,
        comment,
        requires,
        require_count: require_count as u8,
        no_relocate,
        no_relocate_count: no_relocate_count as u8,
    })
}

fn parse_bool_field(field: Option<&str>, what: &'static str) -> Result<bool, CheckpointSetupError> {
    match parse_field::<u8>(field, what)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(CheckpointSetupError::Corrupt("invalid boolean field")),
    }
}

fn next_manifest_word<'a>(
    words: &mut core::str::Split<'a, char>,
    what: &'static str,
) -> Result<&'a str, CheckpointSetupError> {
    words.next().ok_or(CheckpointSetupError::Corrupt(what))
}

fn verify_extension_object_key(
    key: &str,
    prefix: &str,
    suffix: &str,
    bytes: &[u8],
) -> Result<(), CheckpointSetupError> {
    let expected = stack_format!(
        80,
        "{}{:08x}-{}{}",
        prefix,
        crate::wal::crc32c::crc32c(bytes),
        bytes.len(),
        suffix
    );
    if expected.is_truncated() || key != expected.as_str() {
        return Err(CheckpointSetupError::Corrupt(
            "extension object key does not match its content",
        ));
    }
    Ok(())
}

#[inline(never)]
fn finish_pending(
    storage: &mut Storage,
    slot_of: &mut Vec<Option<usize>>,
    pending: &mut Option<(
        usize,
        u64,
        TableDef,
        usize,
        [i64; crate::storage::MAX_COLUMNS],
    )>,
) -> Result<(), CheckpointSetupError> {
    if let Some((manifest_index, created_at, definition, seen, serials)) = pending.take() {
        if seen != definition.n_columns {
            return Err(CheckpointSetupError::Corrupt(
                "manifest column count mismatch",
            ));
        }
        let slot = if storage
            .is_large_object_page_relation(definition.schema.as_str(), definition.name.as_str())
        {
            let slot = storage.large_object_page_table();
            let expected = &*storage.table(slot).definition();
            if manifest_index != slot
                || created_at != 0
                || definition.schema != expected.schema
                || definition.name != expected.name
                || definition.n_columns != expected.n_columns
                || definition.not_null_names != expected.not_null_names
                || definition.has_toast != expected.has_toast
                || definition
                    .columns()
                    .iter()
                    .zip(expected.columns())
                    .any(|(actual, expected)| actual != expected)
            {
                return Err(CheckpointSetupError::Corrupt(
                    "large-object page relation definition mismatch",
                ));
            }
            slot
        } else {
            let position = crate::storage::CatalogPosition::new(manifest_index, created_at)
                .map_err(|_| CheckpointSetupError::Corrupt("invalid table source identity"))?;
            storage
                .restore_table(
                    position,
                    definition,
                    crate::storage::TableCreation::Committed,
                )
                .map_err(|error| {
                    CheckpointSetupError::ObjectStore(format!(
                        "manifest table rejected: {}",
                        error.message.as_str()
                    ))
                })?
        };
        if slot_of
            .get(manifest_index)
            .is_some_and(|entry| entry.is_some())
        {
            return Err(CheckpointSetupError::Corrupt(
                "duplicate table source position",
            ));
        }
        storage.install_table_serial_values(slot, serials);
        if slot_of.len() <= manifest_index {
            slot_of.resize(manifest_index + 1, None);
        }
        slot_of[manifest_index] = Some(slot);
    }
    Ok(())
}

fn load_publication(storage: &mut Storage, line: &str) -> Result<(), CheckpointSetupError> {
    let mut words = line.split(' ');
    let _ = words.next();
    let name = words
        .next()
        .ok_or(CheckpointSetupError::Corrupt("pub name"))
        .and_then(decode_hex_name)?;
    let owner: u16 = parse_field(words.next(), "pub owner")?;
    let created_at = parse_field(words.next(), "pub creation stamp")?;
    let creation = crate::storage::CatalogIdentity::require(created_at)
        .map_err(|_| CheckpointSetupError::Corrupt("invalid publication creation identity"))?;
    let flags: u8 = parse_field(words.next(), "pub flags")?;
    if flags & !0x7f != 0 {
        return Err(CheckpointSetupError::Corrupt("invalid publication flags"));
    }
    let count: usize = parse_field(words.next(), "pub table count")?;
    let schema_count: usize = parse_field(words.next(), "pub schema count")?;
    if count > crate::storage::MAX_PUBLICATION_TABLES {
        return Err(CheckpointSetupError::Corrupt(
            "pub table count exceeds limit",
        ));
    }
    if schema_count > crate::storage::MAX_PUBLICATION_SCHEMAS {
        return Err(CheckpointSetupError::Corrupt(
            "pub schema count exceeds limit",
        ));
    }
    let mut tables = [u16::MAX; crate::storage::MAX_PUBLICATION_TABLES];
    let mut table_column_masks =
        [crate::storage::ColumnSet::EMPTY; crate::storage::MAX_PUBLICATION_TABLES];
    let mut table_include_descendants = [false; crate::storage::MAX_PUBLICATION_TABLES];
    let mut table_filter_sql =
        [crate::util::StackStr::new(); crate::storage::MAX_PUBLICATION_TABLES];
    for index in 0..count {
        tables[index] = parse_field(words.next(), "pub table")?;
        table_column_masks[index] = parse_field(words.next(), "pub table column mask")?;
        table_include_descendants[index] = match parse_field(words.next(), "pub descendants")? {
            0 => false,
            1 => true,
            _ => return Err(CheckpointSetupError::Corrupt("pub descendants")),
        };
        let filter = words
            .next()
            .ok_or(CheckpointSetupError::Corrupt("pub row filter"))?;
        if filter != "-" {
            let decoded = decode_hex_name(filter)?;
            core::fmt::Write::write_str(&mut table_filter_sql[index], decoded.as_str())
                .map_err(|_| CheckpointSetupError::Corrupt("pub row filter exceeds limit"))?;
            if table_filter_sql[index].is_truncated() {
                return Err(CheckpointSetupError::Corrupt(
                    "pub row filter exceeds limit",
                ));
            }
        }
    }
    let mut schemas = [u8::MAX; crate::storage::MAX_PUBLICATION_SCHEMAS];
    for schema in &mut schemas[..schema_count] {
        *schema = parse_field(words.next(), "pub schema")?;
    }
    if words.next().is_some() {
        return Err(CheckpointSetupError::Corrupt("trailing pub fields"));
    }
    let slot = storage
        .create_publication(
            crate::storage::PublicationSpec {
                creation: crate::storage::CatalogCreation::Restore(creation),
                name: sql_name(&name)?,
                all_tables: flags & 1 != 0,
                tables: &tables[..count],
                table_column_masks: &table_column_masks[..count],
                table_include_descendants: &table_include_descendants[..count],
                table_filter_sql: &table_filter_sql[..count],
                schemas: &schemas[..schema_count],
                publish_insert: flags & 2 != 0,
                publish_update: flags & 4 != 0,
                publish_delete: flags & 8 != 0,
                publish_truncate: flags & 16 != 0,
                publish_via_partition_root: flags & 32 != 0,
                publish_generated_columns: if flags & 64 != 0 {
                    crate::storage::PublishGeneratedColumns::Stored
                } else {
                    crate::storage::PublishGeneratedColumns::None
                },
            },
            0,
        )
        .map_err(|error| {
            CheckpointSetupError::ObjectStore(format!(
                "manifest publication rejected: {}",
                error.message.as_str()
            ))
        })?;
    storage.restore_publication_owner(slot, owner);
    storage.commit_publication_create(slot);
    Ok(())
}

fn load_replication_slot(storage: &mut Storage, line: &str) -> Result<(), CheckpointSetupError> {
    let mut words = line.split(' ');
    let _ = words.next();
    let name = words
        .next()
        .ok_or(CheckpointSetupError::Corrupt("replication slot name"))
        .and_then(decode_hex_name)?;
    let restart_lsn = parse_field(words.next(), "replication slot restart LSN")?;
    let confirmed_flush_lsn = parse_field(words.next(), "replication slot confirmed LSN")?;
    let behavior = parse_field(words.next(), "replication slot behavior")?;
    let behavior = crate::storage::ReplicationSlotBehavior::from_code(behavior)
        .ok_or(CheckpointSetupError::Corrupt("replication slot behavior"))?;
    if words.next().is_some() {
        return Err(CheckpointSetupError::Corrupt(
            "trailing replication slot fields",
        ));
    }
    storage
        .restore_replication_slot(
            crate::storage::ReplicationSlotName::parse(&name)
                .map_err(|_| CheckpointSetupError::Corrupt("replication slot name"))?,
            restart_lsn,
            confirmed_flush_lsn,
            behavior,
        )
        .map_err(|error| {
            CheckpointSetupError::ObjectStore(format!(
                "manifest replication slot rejected: {}",
                error.message.as_str()
            ))
        })
}

fn load_policy(storage: &mut Storage, line: &str) -> Result<(), CheckpointSetupError> {
    let mut words = line.split(' ');
    if words.next() != Some("pol") {
        return Err(CheckpointSetupError::Corrupt("policy record"));
    }
    let slot = parse_field(words.next(), "policy position")?;
    let created_at = parse_field(words.next(), "policy created_at")?;
    let identity = crate::storage::CatalogPosition::new(slot, created_at)
        .map_err(|_| CheckpointSetupError::Corrupt("policy position"))?;
    let command =
        crate::storage::PolicyCommandKind::from_code(parse_field(words.next(), "policy command")?)
            .ok_or(CheckpointSetupError::Corrupt("policy command"))?;
    let permissive = match parse_field::<u8>(words.next(), "policy permissiveness")? {
        0 => false,
        1 => true,
        _ => return Err(CheckpointSetupError::Corrupt("policy permissiveness")),
    };
    let schema = decode_hex_name(
        words
            .next()
            .ok_or(CheckpointSetupError::Corrupt("policy schema"))?,
    )?;
    let table_name = decode_hex_name(
        words
            .next()
            .ok_or(CheckpointSetupError::Corrupt("policy table"))?,
    )?;
    let name = sql_name(&decode_hex_name(
        words
            .next()
            .ok_or(CheckpointSetupError::Corrupt("policy name"))?,
    )?)?;
    let role_count: usize = parse_field(words.next(), "policy role count")?;
    if role_count == 0 || role_count > storage.policy_role_capacity() {
        return Err(CheckpointSetupError::Corrupt("policy role count"));
    }
    let mut roles = Vec::with_capacity(role_count);
    for _ in 0..role_count {
        roles.push(decode_hex_name(
            words
                .next()
                .ok_or(CheckpointSetupError::Corrupt("policy role"))?,
        )?);
    }
    let expression = |value: Option<&str>| {
        let value = value.ok_or(CheckpointSetupError::Corrupt("policy expression"))?;
        if value == "-" {
            Ok(None)
        } else {
            crate::storage::policy_expression(&decode_hex_name(value)?)
                .map(Some)
                .map_err(|_| CheckpointSetupError::Corrupt("policy expression"))
        }
    };
    let using = expression(words.next())?;
    let with_check = expression(words.next())?;
    let dependencies = parse_stored_query_dependencies(&mut words)?;
    if words.next().is_some() {
        return Err(CheckpointSetupError::Corrupt("trailing policy fields"));
    }
    let table = match storage.resolve_relation(Some(&schema), &table_name, 0) {
        Some(crate::storage::ResolvedRelation::Table(slot)) => slot,
        _ => return Err(CheckpointSetupError::Corrupt("policy table does not exist")),
    };
    storage
        .replay_set_policy(
            identity,
            crate::storage::RecoveredPolicySpec {
                name,
                table,
                command,
                permissive,
                definition: crate::storage::PolicyDefinition { using, with_check },
            },
            roles.iter().map(String::as_str),
            dependencies,
        )
        .map_err(|error| {
            CheckpointSetupError::ObjectStore(format!(
                "manifest policy rejected: {}",
                error.message.as_str()
            ))
        })
        .map(|_| ())
}

fn load_trigger(storage: &mut Storage, line: &str) -> Result<(), CheckpointSetupError> {
    let mut words = line.split_ascii_whitespace();
    if words.next() != Some("trg") {
        return Err(CheckpointSetupError::Corrupt("trigger record"));
    }
    let slot = parse_field(words.next(), "trigger position")?;
    let created_at = parse_field(words.next(), "trigger created_at")?;
    let identity = crate::storage::CatalogPosition::new(slot, created_at)
        .map_err(|_| CheckpointSetupError::Corrupt("trigger position"))?;
    let target_kind: u8 = parse_field(words.next(), "trigger target")?;
    let name = sql_name(&decode_hex_name(
        words
            .next()
            .ok_or(CheckpointSetupError::Corrupt("trigger name"))?,
    )?)?;
    let schema = decode_hex_name(
        words
            .next()
            .ok_or(CheckpointSetupError::Corrupt("trigger table schema"))?,
    )?;
    let table_name = decode_hex_name(
        words
            .next()
            .ok_or(CheckpointSetupError::Corrupt("trigger table"))?,
    )?;
    let function_schema = decode_hex_name(
        words
            .next()
            .ok_or(CheckpointSetupError::Corrupt("trigger function schema"))?,
    )?;
    let function_name = decode_hex_name(
        words
            .next()
            .ok_or(CheckpointSetupError::Corrupt("trigger function"))?,
    )?;
    let timing =
        crate::sql::ast::TriggerTiming::from_code(parse_field(words.next(), "trigger timing")?)
            .ok_or(CheckpointSetupError::Corrupt("trigger timing"))?;
    let level =
        crate::sql::ast::TriggerLevel::from_code(parse_field(words.next(), "trigger level")?)
            .ok_or(CheckpointSetupError::Corrupt("trigger level"))?;
    let events =
        crate::sql::ast::TriggerEvents::from_bits(parse_field(words.next(), "trigger events")?)
            .ok_or(CheckpointSetupError::Corrupt("trigger events"))?;
    let update_columns: crate::storage::ColumnSet =
        parse_field(words.next(), "trigger update columns")?;
    let old_table = match words
        .next()
        .ok_or(CheckpointSetupError::Corrupt("trigger old table"))?
    {
        "-" => None,
        value => Some(decode_hex_name(value)?),
    };
    let new_table = match words
        .next()
        .ok_or(CheckpointSetupError::Corrupt("trigger new table"))?
    {
        "-" => None,
        value => Some(decode_hex_name(value)?),
    };
    let transition_tables = crate::storage::TriggerTransitionTables::from_names(
        old_table.as_deref(),
        new_table.as_deref(),
    )
    .ok_or(CheckpointSetupError::Corrupt(
        "duplicate trigger transition table names",
    ))?;
    let when = match words
        .next()
        .ok_or(CheckpointSetupError::Corrupt("trigger when"))?
    {
        "-" => None,
        value => Some(
            crate::storage::trigger_when_stackstr(&decode_hex_name(value)?).map_err(|error| {
                CheckpointSetupError::ObjectStore(format!(
                    "manifest trigger rejected: {}",
                    error.message.as_str()
                ))
            })?,
        ),
    };
    let argument_field = words
        .next()
        .ok_or(CheckpointSetupError::Corrupt("trigger arguments"))?;
    let bytes = argument_field.as_bytes();
    let hex_byte = |at: &mut usize| -> Result<usize, CheckpointSetupError> {
        let value = core::str::from_utf8(
            bytes
                .get(*at..*at + 2)
                .ok_or(CheckpointSetupError::Corrupt("trigger argument length"))?,
        )
        .ok()
        .and_then(|value| usize::from_str_radix(value, 16).ok())
        .ok_or(CheckpointSetupError::Corrupt("trigger argument length"))?;
        *at += 2;
        Ok(value)
    };
    let mut argument_at = 0usize;
    let argument_count = hex_byte(&mut argument_at)?;
    if argument_count > crate::storage::MAX_TRIGGER_ARGUMENTS {
        return Err(CheckpointSetupError::Corrupt("trigger argument count"));
    }
    let mut argument_values =
        [crate::util::StackStr::<{ crate::storage::TRIGGER_ARGUMENT_BYTES }>::new();
            crate::storage::MAX_TRIGGER_ARGUMENTS];
    for value in argument_values.iter_mut().take(argument_count) {
        let length = hex_byte(&mut argument_at)?;
        if length > crate::storage::TRIGGER_ARGUMENT_BYTES || argument_at + length * 2 > bytes.len()
        {
            return Err(CheckpointSetupError::Corrupt("trigger argument"));
        }
        *value = crate::util::StackStr::from_str(&decode_hex_name(
            core::str::from_utf8(&bytes[argument_at..argument_at + length * 2])
                .map_err(|_| CheckpointSetupError::Corrupt("trigger argument"))?,
        )?);
        argument_at += length * 2;
        if value.is_truncated() {
            return Err(CheckpointSetupError::Corrupt("trigger argument"));
        }
    }
    if argument_at != bytes.len() {
        return Err(CheckpointSetupError::Corrupt(
            "trigger argument trailing data",
        ));
    }
    let mut argument_refs = [""; crate::storage::MAX_TRIGGER_ARGUMENTS];
    for (index, value) in argument_values.iter().take(argument_count).enumerate() {
        argument_refs[index] = value.as_str();
    }
    let arguments = crate::storage::TriggerArguments::parse(&argument_refs[..argument_count])
        .map_err(|error| {
            CheckpointSetupError::ObjectStore(format!(
                "manifest trigger rejected: {}",
                error.message.as_str()
            ))
        })?;
    let constraint = match parse_field::<u8>(words.next(), "trigger kind")? {
        0 => false,
        1 => true,
        _ => return Err(CheckpointSetupError::Corrupt("trigger kind")),
    };
    let constraint_timing = crate::storage::ConstraintTiming::from_code(parse_field::<u8>(
        words.next(),
        "constraint trigger timing",
    )?)
    .ok_or(CheckpointSetupError::Corrupt("constraint trigger timing"))?;
    let referenced_schema = match words
        .next()
        .ok_or(CheckpointSetupError::Corrupt("trigger referenced schema"))?
    {
        "-" => None,
        value => Some(decode_hex_name(value)?),
    };
    let referenced_table = match words
        .next()
        .ok_or(CheckpointSetupError::Corrupt("trigger referenced table"))?
    {
        "-" => None,
        value => Some(decode_hex_name(value)?),
    };
    let enabled = crate::storage::TriggerEnabled::from_code(parse_field::<u8>(
        words.next(),
        "trigger enabled",
    )?)
    .ok_or(CheckpointSetupError::Corrupt("trigger enabled"))?;
    if (matches!(level, crate::sql::ast::TriggerLevel::Row) && events.has_truncate())
        || !transition_tables.is_valid_for(timing, level, events)
        || (!matches!(
            transition_tables,
            crate::storage::TriggerTransitionTables::None
        ) && !update_columns.is_empty())
        || words.next().is_some()
    {
        return Err(CheckpointSetupError::Corrupt("malformed trigger record"));
    }
    let target = match (
        target_kind,
        storage.resolve_relation(Some(&schema), &table_name, 0),
    ) {
        (0, Some(crate::storage::ResolvedRelation::Table(slot))) => {
            crate::storage::TriggerTarget::Table(slot as u16)
        }
        (1, Some(crate::storage::ResolvedRelation::View(slot))) => {
            crate::storage::TriggerTarget::View(slot as u16)
        }
        _ => {
            return Err(CheckpointSetupError::Corrupt(
                "trigger relation does not exist",
            ));
        }
    };
    let kind = if constraint {
        let referenced_table = match (referenced_schema.as_deref(), referenced_table.as_deref()) {
            (Some(schema), Some(table)) => match storage.resolve_relation(Some(schema), table, 0) {
                Some(crate::storage::ResolvedRelation::Table(slot)) => Some(slot as u16),
                _ => {
                    return Err(CheckpointSetupError::Corrupt(
                        "constraint trigger referenced table does not exist",
                    ));
                }
            },
            (None, None) => None,
            _ => {
                return Err(CheckpointSetupError::Corrupt(
                    "constraint trigger referenced table is incomplete",
                ));
            }
        };
        crate::storage::TriggerKind::Constraint {
            referenced_table,
            timing: constraint_timing,
        }
    } else if constraint_timing != crate::storage::ConstraintTiming::NotDeferrable
        || referenced_schema.is_some()
        || referenced_table.is_some()
    {
        return Err(CheckpointSetupError::Corrupt(
            "ordinary trigger carries constraint state",
        ));
    } else {
        crate::storage::TriggerKind::Ordinary
    };
    if !crate::storage::trigger_shape_is_valid(
        matches!(target, crate::storage::TriggerTarget::View(_)),
        matches!(kind, crate::storage::TriggerKind::Constraint { .. }),
        timing,
        level,
        events,
        update_columns,
        transition_tables,
    ) {
        return Err(CheckpointSetupError::Corrupt(
            "invalid trigger relation or timing",
        ));
    }
    let function = storage
        .routine_slot_by_signature(&function_schema, &function_name, &[], 0)
        .ok_or(CheckpointSetupError::Corrupt(
            "trigger function does not exist",
        ))?;
    if !matches!(
        storage.routine(function).kind,
        crate::storage::RoutineKind::Trigger
    ) {
        return Err(CheckpointSetupError::Corrupt(
            "trigger function is not trigger typed",
        ));
    }
    storage
        .restore_trigger(
            identity,
            crate::storage::TriggerSpec {
                name,
                target,
                kind,
                function,
                timing,
                level,
                events,
                update_columns,
                transition_tables,
                when,
                arguments,
            },
            enabled,
        )
        .map_err(|error| {
            CheckpointSetupError::ObjectStore(format!(
                "manifest trigger rejected: {}",
                error.message.as_str()
            ))
        })?;
    Ok(())
}

fn load_partition_trigger_state(
    storage: &mut Storage,
    line: &str,
) -> Result<(), CheckpointSetupError> {
    let mut words = line.split_ascii_whitespace();
    if words.next() != Some("trgs") {
        return Err(CheckpointSetupError::Corrupt(
            "partition trigger state record",
        ));
    }
    let creation = crate::storage::CatalogIdentity::parse(parse_field(
        words.next(),
        "inherited trigger identity",
    )?)
    .ok_or(CheckpointSetupError::Corrupt("inherited trigger identity"))?;
    let trigger = crate::storage::CatalogPosition::new(
        parse_field(words.next(), "inherited trigger parent position")?,
        parse_field(words.next(), "inherited trigger parent identity")?,
    )
    .map_err(|_| CheckpointSetupError::Corrupt("inherited trigger parent"))?;
    crate::storage::CatalogIdentity::require(trigger.created_at())
        .map_err(|_| CheckpointSetupError::Corrupt("inherited trigger parent identity"))?;
    let table = crate::storage::CatalogPosition::new(
        parse_field(words.next(), "inherited trigger table position")?,
        parse_field(words.next(), "inherited trigger table identity")?,
    )
    .map_err(|_| CheckpointSetupError::Corrupt("inherited trigger table"))?;
    let enabled = crate::storage::TriggerEnabled::from_code(parse_field::<u8>(
        words.next(),
        "inherited trigger enabled",
    )?)
    .ok_or(CheckpointSetupError::Corrupt("inherited trigger enabled"))?;
    if words.next().is_some() {
        return Err(CheckpointSetupError::Corrupt(
            "malformed inherited trigger state",
        ));
    }
    storage
        .install_partition_trigger_image(
            None,
            Some(crate::storage::PartitionTriggerImage {
                creation,
                trigger,
                table,
                enabled,
            }),
            0,
        )
        .map_err(|error| {
            CheckpointSetupError::ObjectStore(format!(
                "manifest inherited trigger rejected: {}",
                error.message.as_str()
            ))
        })
}

fn load_subscription(storage: &mut Storage, line: &str) -> Result<(), CheckpointSetupError> {
    let mut words = line.split(' ');
    let _ = words.next();
    let name = words
        .next()
        .ok_or(CheckpointSetupError::Corrupt("subscription name"))
        .and_then(decode_hex_name)?;
    let owner = parse_field(words.next(), "subscription owner")?;
    let enabled = match parse_field::<u8>(words.next(), "subscription enabled")? {
        0 => false,
        1 => true,
        _ => {
            return Err(CheckpointSetupError::Corrupt(
                "invalid subscription enabled flag",
            ));
        }
    };
    let connection = words
        .next()
        .ok_or(CheckpointSetupError::Corrupt("subscription connection"))
        .and_then(decode_hex_name)
        .and_then(|value| {
            crate::storage::SubscriptionConnInfo::parse(&value)
                .map_err(|_| CheckpointSetupError::Corrupt("invalid subscription connection"))
        })?;
    let slot_kind = parse_field::<u8>(words.next(), "subscription slot kind")?;
    let slot_word = words
        .next()
        .ok_or(CheckpointSetupError::Corrupt("subscription slot name"))?;
    let publisher_slot = match slot_kind {
        0 if slot_word == "-" => crate::storage::SubscriptionSlot::Absent,
        1 => crate::storage::SubscriptionSlot::External(decode_hex_name(slot_word).and_then(
            |value| {
                crate::storage::ReplicationSlotName::parse(&value)
                    .map_err(|_| CheckpointSetupError::Corrupt("subscription slot name"))
            },
        )?),
        2 => crate::storage::SubscriptionSlot::Managed(decode_hex_name(slot_word).and_then(
            |value| {
                crate::storage::ReplicationSlotName::parse(&value)
                    .map_err(|_| CheckpointSetupError::Corrupt("subscription slot name"))
            },
        )?),
        _ => return Err(CheckpointSetupError::Corrupt("subscription slot kind")),
    };
    let bootstrap = crate::storage::SubscriptionBootstrap::from_code(parse_field::<u8>(
        words.next(),
        "subscription bootstrap state",
    )?)
    .ok_or(CheckpointSetupError::Corrupt(
        "subscription bootstrap state",
    ))?;
    let subscription_flag = |word, field| match parse_field::<u8>(word, field)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(CheckpointSetupError::Corrupt(field)),
    };
    let binary = subscription_flag(words.next(), "subscription binary setting")?;
    let streaming = crate::storage::SubscriptionStreaming::from_code(parse_field::<u8>(
        words.next(),
        "subscription streaming setting",
    )?)
    .ok_or(CheckpointSetupError::Corrupt(
        "subscription streaming setting",
    ))?;
    let synchronous_commit = crate::storage::SubscriptionSynchronousCommit::from_code(
        parse_field::<u8>(words.next(), "subscription synchronous commit setting")?,
    )
    .ok_or(CheckpointSetupError::Corrupt(
        "subscription synchronous commit setting",
    ))?;
    let two_phase = subscription_flag(words.next(), "subscription two phase setting")?;
    let disable_on_error =
        subscription_flag(words.next(), "subscription disable on error setting")?;
    let password_required =
        subscription_flag(words.next(), "subscription password required setting")?;
    let run_as_owner = subscription_flag(words.next(), "subscription run as owner setting")?;
    let origin = crate::storage::SubscriptionOrigin::from_code(parse_field::<u8>(
        words.next(),
        "subscription origin setting",
    )?)
    .ok_or(CheckpointSetupError::Corrupt("subscription origin setting"))?;
    let failover = subscription_flag(words.next(), "subscription failover setting")?;
    let skip_present = subscription_flag(words.next(), "subscription skip LSN setting")?;
    let skip_value = parse_field::<u64>(words.next(), "subscription skip LSN")?;
    let behavior = crate::storage::SubscriptionBehavior {
        binary,
        streaming,
        synchronous_commit,
        two_phase,
        disable_on_error,
        password_required,
        run_as_owner,
        origin,
        failover,
        skip_lsn: skip_present.then_some(skip_value),
    };
    let created_at = parse_field(words.next(), "subscription creation stamp")?;
    let definition_generation = parse_field(words.next(), "subscription definition generation")?;
    let confirmed_lsn = parse_field(words.next(), "subscription confirmed LSN")?;
    let origin_lsn = parse_field(words.next(), "subscription origin local LSN")?;
    if (confirmed_lsn == 0) != (origin_lsn == 0) {
        return Err(CheckpointSetupError::Corrupt(
            "subscription origin position",
        ));
    }
    let cleanup = match parse_field::<u8>(words.next(), "subscription cleanup state")? {
        0 => false,
        1 => true,
        _ => {
            return Err(CheckpointSetupError::Corrupt("subscription cleanup state"));
        }
    };
    let failure = match parse_field::<u8>(words.next(), "subscription failure state")? {
        0 => {
            if words.next() != Some("-") || words.next() != Some("-") {
                return Err(CheckpointSetupError::Corrupt("subscription failure state"));
            }
            None
        }
        1 => {
            let code = words
                .next()
                .and_then(crate::sql::eval::SqlState::parse)
                .ok_or(CheckpointSetupError::Corrupt(
                    "subscription failure SQLSTATE",
                ))?;
            let message = words
                .next()
                .ok_or(CheckpointSetupError::Corrupt(
                    "subscription failure message",
                ))
                .and_then(decode_hex_name)?;
            if message.is_empty() {
                return Err(CheckpointSetupError::Corrupt(
                    "subscription failure message",
                ));
            }
            Some(crate::storage::SubscriptionFailure {
                sqlstate: code,
                message: StackStr::from_str(&message),
            })
        }
        _ => return Err(CheckpointSetupError::Corrupt("subscription failure state")),
    };
    let count: usize = parse_field(words.next(), "subscription publication count")?;
    if count == 0 || count > crate::storage::MAX_SUBSCRIPTION_PUBLICATIONS {
        return Err(CheckpointSetupError::Corrupt(
            "subscription publication count exceeds limit",
        ));
    }
    let mut publications = [SqlName::EMPTY; crate::storage::MAX_SUBSCRIPTION_PUBLICATIONS];
    for publication in &mut publications[..count] {
        *publication = words
            .next()
            .ok_or(CheckpointSetupError::Corrupt("subscription publication"))
            .and_then(decode_hex_name)
            .and_then(|value| sql_name(&value))?;
    }
    if words.next().is_some() {
        return Err(CheckpointSetupError::Corrupt(
            "trailing subscription fields",
        ));
    }
    let name = sql_name(&name)?;
    if enabled {
        connection
            .require_endpoint_for(name)
            .map_err(|_| CheckpointSetupError::Corrupt("enabled subscription endpoint"))?;
    }
    let slot = storage
        .create_subscription(
            crate::storage::SubscriptionSpec {
                name,
                connection,
                publications: &publications[..count],
                enabled,
                slot: publisher_slot,
                behavior,
                bootstrap,
            },
            0,
        )
        .map_err(|error| {
            CheckpointSetupError::ObjectStore(format!(
                "manifest subscription rejected: {}",
                error.message.as_str()
            ))
        })?;
    storage.restore_subscription_owner(slot, owner);
    storage.commit_subscription_create(slot);
    storage
        .restore_subscription_stream_identity(slot, created_at, definition_generation)
        .map_err(|error| {
            CheckpointSetupError::ObjectStore(format!(
                "manifest subscription stream identity rejected: {}",
                error.message.as_str()
            ))
        })?;
    if confirmed_lsn != 0 {
        let stream = storage
            .subscription_stream(slot, 0)
            .ok_or(CheckpointSetupError::Corrupt(
                "subscription stream identity",
            ))?;
        let advance = storage
            .subscription_advance(stream, confirmed_lsn, 0)
            .map_err(|error| {
                CheckpointSetupError::ObjectStore(format!(
                    "manifest subscription position rejected: {}",
                    error.message.as_str()
                ))
            })?
            .ok_or(CheckpointSetupError::Corrupt(
                "subscription position did not advance",
            ))?;
        storage.apply_subscription_advance(advance, origin_lsn);
    }
    if cleanup {
        let dropped = storage
            .drop_subscription(name.as_str(), 0)
            .map_err(|error| {
                CheckpointSetupError::ObjectStore(format!(
                    "manifest subscription cleanup rejected: {}",
                    error.message.as_str()
                ))
            })?
            .ok_or(CheckpointSetupError::Corrupt("subscription cleanup target"))?;
        storage.commit_subscription_drop(dropped);
    } else if let Some(failure) = failure {
        let stream = storage
            .subscription_stream(slot, 0)
            .ok_or(CheckpointSetupError::Corrupt("subscription failure target"))?;
        storage
            .fail_subscription(stream, failure)
            .map_err(|_| CheckpointSetupError::Corrupt("subscription failure target"))?;
    }
    Ok(())
}

fn load_subscription_relation(
    storage: &mut Storage,
    line: &str,
) -> Result<(), CheckpointSetupError> {
    let mut words = line.split(' ');
    let _ = words.next();
    let created_at = parse_field(words.next(), "subscription relation creation stamp")?;
    let definition_generation =
        parse_field(words.next(), "subscription relation definition generation")?;
    let schema = words
        .next()
        .ok_or(CheckpointSetupError::Corrupt(
            "subscription relation schema",
        ))
        .and_then(decode_hex_name)?;
    let table = words
        .next()
        .ok_or(CheckpointSetupError::Corrupt("subscription relation table"))
        .and_then(decode_hex_name)?;
    let state = crate::storage::SubscriptionRelationState::from_code(parse_field::<u8>(
        words.next(),
        "subscription relation state",
    )?)
    .ok_or(CheckpointSetupError::Corrupt("subscription relation state"))?;
    let synchronization_lsn =
        parse_field(words.next(), "subscription relation synchronization LSN")?;
    if words.next().is_some() {
        return Err(CheckpointSetupError::Corrupt(
            "trailing subscription relation fields",
        ));
    }
    let slot = storage
        .subscriptions_with_slots_visible_to(Some(storage.current_database_oid()), 0)
        .find(|(_, subscription)| {
            subscription.created_at == created_at
                && subscription.definition_generation == definition_generation
        })
        .map(|(slot, _)| slot)
        .ok_or(CheckpointSetupError::Corrupt(
            "subscription relation stream identity",
        ))?;
    let stream = storage
        .subscription_stream(slot, 0)
        .ok_or(CheckpointSetupError::Corrupt(
            "subscription relation stream identity",
        ))?;
    storage
        .restore_subscription_relation(stream, &schema, &table, state, synchronization_lsn)
        .map_err(|error| {
            CheckpointSetupError::ObjectStore(format!(
                "manifest subscription relation rejected: {}",
                error.message.as_str()
            ))
        })
}

#[inline(never)]
fn load_view(
    storage: &mut Storage,
    line: &str,
    catalog_sequence: u64,
) -> Result<(), CheckpointSetupError> {
    let mut words = line.split(' ');
    if words.next() != Some("vw13") {
        return Err(CheckpointSetupError::Corrupt("unsupported view record"));
    }
    let identity = crate::storage::ViewCatalogIdentity::new(
        parse_field(words.next(), "view slot missing")?,
        parse_field(words.next(), "view creation identity missing")?,
        parse_field(words.next(), "view return-rule slot missing")?,
        parse_field(words.next(), "view return-rule identity missing")?,
    )
    .ok_or(CheckpointSetupError::Corrupt(
        "invalid view catalog identity",
    ))?;
    if identity.created_at() > catalog_sequence
        || identity.return_rule_created_at() > catalog_sequence
    {
        return Err(CheckpointSetupError::Corrupt(
            "view source exceeds saved identity allocator",
        ));
    }
    let read_hex = |word: Option<&str>, what: &'static str| {
        word.ok_or(CheckpointSetupError::Corrupt(what))
            .and_then(decode_hex_name)
    };
    let sql = read_hex(words.next(), "view sql missing")?;
    let schema = read_hex(words.next(), "view schema missing")?;
    let path = read_hex(words.next(), "view path missing")?;
    let name = read_hex(words.next(), "view name missing")?;
    let security = match parse_field::<u8>(words.next(), "view security missing")? {
        0 => crate::storage::ViewSecurity::Definer,
        1 => crate::storage::ViewSecurity::Invoker,
        _ => return Err(CheckpointSetupError::Corrupt("invalid view security")),
    };
    let security_barrier = crate::storage::ViewSecurityBarrier::from_code(parse_field(
        words.next(),
        "view security barrier missing",
    )?)
    .ok_or(CheckpointSetupError::Corrupt(
        "invalid view security barrier",
    ))?;
    let check_option = match parse_field::<u8>(words.next(), "view check option missing")? {
        0 => None,
        code => crate::storage::ViewCheckOption::from_code(code)
            .ok_or(CheckpointSetupError::Corrupt("invalid view check option"))
            .map(Some)?,
    };
    let encoded_columns = words
        .next()
        .ok_or(CheckpointSetupError::Corrupt("view columns missing"))?;
    let mut encoded_columns = encoded_columns.splitn(3, ':');
    let aliases = match parse_field::<u8>(encoded_columns.next(), "view column aliases")? {
        0 => false,
        1 => true,
        _ => return Err(CheckpointSetupError::Corrupt("invalid view column aliases")),
    };
    let count = encoded_columns
        .next()
        .ok_or(CheckpointSetupError::Corrupt("invalid view columns"))?;
    let encoded_entries = encoded_columns
        .next()
        .ok_or(CheckpointSetupError::Corrupt("invalid view columns"))?;
    let count = parse_field::<usize>(Some(count), "view column count")?;
    if count > crate::storage::MAX_RELATION_COLUMNS {
        return Err(CheckpointSetupError::Corrupt("view column count"));
    }
    let mut names = [crate::storage::SqlName::EMPTY; crate::storage::MAX_COLUMNS];
    let mut defaults = [ColumnDefault::NONE; crate::storage::MAX_COLUMNS];
    if count == 0 {
        if !encoded_entries.is_empty() {
            return Err(CheckpointSetupError::Corrupt("invalid empty view columns"));
        }
    } else {
        let mut entries = encoded_entries.split(',');
        for index in 0..count {
            let entry = entries
                .next()
                .ok_or(CheckpointSetupError::Corrupt("view column missing"))?;
            let mut fields = entry.splitn(4, '/');
            names[index] =
                sql_name(&decode_hex_name(fields.next().ok_or(
                    CheckpointSetupError::Corrupt("view column name missing"),
                )?)?)?;
            let tag = parse_field::<u8>(fields.next(), "view column default kind")?;
            let value = fields.next().ok_or(CheckpointSetupError::Corrupt(
                "view column default value missing",
            ))?;
            let expression = fields.next().ok_or(CheckpointSetupError::Corrupt(
                "view column default expression missing",
            ))?;
            let expression = if expression == "0" {
                None
            } else {
                let source = decode_hex_name(expression)?;
                let expression = StackStr::from_str(&source);
                if expression.is_truncated() {
                    return Err(CheckpointSetupError::Corrupt(
                        "view column default expression exceeds limit",
                    ));
                }
                Some(expression)
            };
            defaults[index] = match tag {
                0 if value == "0" && expression.is_none() => ColumnDefault::NONE,
                1 => ColumnDefault::from_parts(default_from_hex(value)?, expression, false).ok_or(
                    CheckpointSetupError::Corrupt("invalid view constant default"),
                )?,
                2 if value == "0" => ColumnDefault::from_parts(None, expression, false).ok_or(
                    CheckpointSetupError::Corrupt("invalid view expression default"),
                )?,
                _ => return Err(CheckpointSetupError::Corrupt("invalid view column default")),
            };
        }
        if entries.next().is_some() {
            return Err(CheckpointSetupError::Corrupt("trailing view columns"));
        }
    }
    let mut columns = crate::storage::ViewColumns::from_sql_names(&names[..count])
        .map(|columns| columns.with_aliases(aliases))
        .map_err(|error| {
            CheckpointSetupError::ObjectStore(format!(
                "manifest view columns rejected: {}",
                error.message.as_str()
            ))
        })?;
    for (index, default) in defaults[..count].iter().copied().enumerate() {
        columns = columns.with_default(index, default).map_err(|error| {
            CheckpointSetupError::ObjectStore(format!(
                "manifest view default rejected: {}",
                error.message.as_str()
            ))
        })?;
    }
    let dependencies = parse_stored_query_dependencies(&mut words)?;
    use core::fmt::Write;
    let mut buffer = StackStr::<{ crate::storage::VIEW_SQL_MAX }>::new();
    let _ = write!(buffer, "{sql}");
    let mut path_buffer = StackStr::<128>::new();
    let _ = write!(path_buffer, "{path}");
    let definition = crate::storage::ViewDefinition {
        persistence: crate::storage::RelationPersistence::Permanent,
        columns,
        query: crate::storage::StoredQueryDefinition {
            sql: buffer,
            creation_path: path_buffer,
            dependencies,
        },
        options: crate::storage::ViewOptions {
            security,
            security_barrier,
            check_option,
        },
    };
    storage
        .replay_view(identity, sql_name(&schema)?, sql_name(&name)?, definition)
        .map_err(|error| {
            CheckpointSetupError::ObjectStore(format!(
                "manifest view rejected: {}",
                error.message.as_str()
            ))
        })?;
    Ok(())
}

#[inline(never)]
fn parse_catalog_position(
    words: &mut core::str::Split<'_, char>,
    catalog_sequence: u64,
) -> Result<crate::storage::CatalogPosition, CheckpointSetupError> {
    let slot = parse_field(words.next(), "catalog source slot")?;
    let created_at = parse_field(words.next(), "catalog source creation")?;
    if created_at > catalog_sequence {
        return Err(CheckpointSetupError::Corrupt(
            "catalog source exceeds saved identity allocator",
        ));
    }
    crate::storage::CatalogPosition::new(slot, created_at)
        .map_err(|_| CheckpointSetupError::Corrupt("invalid catalog source identity"))
}

#[inline(never)]
fn load_matview(
    storage: &mut Storage,
    line: &str,
    catalog_sequence: u64,
) -> Result<(), CheckpointSetupError> {
    let mut words = line.split(' ');
    let _tag = words.next();
    let position = parse_catalog_position(&mut words, catalog_sequence)?;
    let backing = parse_catalog_position(&mut words, catalog_sequence)?;
    let read_hex = |word: Option<&str>, what: &'static str| {
        word.ok_or(CheckpointSetupError::Corrupt(what))
            .and_then(decode_hex_name)
    };
    let sql = read_hex(words.next(), "materialized-view sql missing")?;
    let schema = read_hex(words.next(), "materialized-view schema missing")?;
    let path = read_hex(words.next(), "materialized-view path missing")?;
    let name = read_hex(words.next(), "materialized-view name missing")?;
    let populated: u8 = parse_field(words.next(), "materialized-view populated")?;
    let dependencies = parse_stored_query_dependencies(&mut words)?;
    if populated > 1 || words.next().is_some() {
        return Err(CheckpointSetupError::Corrupt(
            "invalid materialized-view fields",
        ));
    }
    use core::fmt::Write;
    let mut buffer = StackStr::<{ crate::storage::VIEW_SQL_MAX }>::new();
    let _ = write!(buffer, "{sql}");
    let mut path_buffer = StackStr::<128>::new();
    let _ = write!(path_buffer, "{path}");
    let backing_table = backing.slot();
    if backing_table >= storage.table_count()
        || !storage.table_slot_visible_to(backing_table, 0)
        || storage.table(backing_table).created_at() != backing.created_at()
        || {
            let definition = storage.table_def(backing_table, 0);
            definition.schema.as_str() != schema || definition.name.as_str() != name
        }
    {
        return Err(CheckpointSetupError::Corrupt(
            "materialized-view backing relation identity",
        ));
    }
    let slot = storage
        .create_matview(
            crate::storage::CatalogAllocation::Restore(position),
            backing_table,
            crate::storage::StoredQueryDefinition {
                sql: buffer,
                creation_path: path_buffer,
                dependencies,
            },
            populated != 0,
            0,
        )
        .map_err(|error| {
            CheckpointSetupError::ObjectStore(format!(
                "manifest matview rejected: {}",
                error.message.as_str()
            ))
        })?;
    storage.finish_matview_change(slot, crate::storage::CatalogDdlDecision::Commit);
    Ok(())
}

struct ManifestDependencies<'a>(crate::storage::StoredQueryDependencyView<'a>);

struct ManifestViewColumns(crate::storage::ViewColumns);

struct ManifestName<'a>(&'a str);

struct ManifestPolicyRoles<'a> {
    storage: &'a Storage,
    roles: &'a [u16],
}

impl core::fmt::Display for ManifestPolicyRoles<'_> {
    fn fmt(&self, output: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for role in self.roles {
            if *role == crate::storage::PUBLIC_ROLE {
                write!(output, " {}", ManifestName("public"))?;
            } else {
                let name = self.storage.role_name(usize::from(*role), 0);
                write!(output, " {}", ManifestName(name.as_str()))?;
            }
        }
        Ok(())
    }
}

/// Older manifest records use a single zero for an absent optional name.
struct ManifestZeroName<'a>(&'a str);

// Stream variable-width routine fields into the one startup-reserved manifest
// buffer. No intermediate field can truncate and publish an unreadable record.
struct ManifestRoutineArguments<'a>(&'a [crate::storage::RoutineArgumentDef]);
struct ManifestRoutineParameters<'a>(&'a [crate::storage::RoutineParameterDef]);
struct ManifestRoutineConfigs<'a>(&'a [crate::storage::RoutineConfig]);
struct ManifestRoutineColumns<'a>(&'a crate::storage::RoutineDef);

fn write_manifest_routine_type(
    output: &mut core::fmt::Formatter<'_>,
    name: crate::storage::SqlName,
    ctype: ColType,
    user_type: Option<crate::storage::UserTypeName>,
) -> core::fmt::Result {
    write!(output, " {} {}", ManifestName(name.as_str()), ctype.code())?;
    match user_type {
        Some(identity) => write!(
            output,
            " {} {}",
            ManifestName(identity.schema.as_str()),
            ManifestName(identity.storage_name().as_str())
        ),
        None => output.write_str(" - -"),
    }
}

impl core::fmt::Display for ManifestRoutineArguments<'_> {
    fn fmt(&self, output: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for argument in self.0 {
            write_manifest_routine_type(output, argument.name, argument.ctype, argument.user_type)?;
        }
        Ok(())
    }
}

impl core::fmt::Display for ManifestRoutineParameters<'_> {
    fn fmt(&self, output: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(output, " {}", self.0.len())?;
        for parameter in self.0 {
            write_manifest_routine_type(
                output,
                parameter.name,
                parameter.ctype,
                parameter.user_type,
            )?;
            let default = parameter.mode.default();
            write!(
                output,
                " {} {}",
                parameter.mode.code(),
                ManifestName(default.as_ref().map_or("", |value| value.as_str()))
            )?;
        }
        Ok(())
    }
}

impl core::fmt::Display for ManifestRoutineConfigs<'_> {
    fn fmt(&self, output: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(output, " {}", self.0.len())?;
        for config in self.0 {
            write!(
                output,
                " {} {}",
                ManifestName(config.name.as_str()),
                ManifestName(config.value.as_str())
            )?;
        }
        Ok(())
    }
}

impl core::fmt::Display for ManifestRoutineColumns<'_> {
    fn fmt(&self, output: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if matches!(
            self.0.kind,
            crate::storage::RoutineKind::TableFunction
                | crate::storage::RoutineKind::RecordFunction { .. }
        ) {
            write!(output, " {}", self.0.result_column_count)?;
            ManifestRoutineArguments(&self.0.result_columns[..self.0.result_column_count])
                .fmt(output)?;
        }
        Ok(())
    }
}

fn text_search_behavior_code(behavior: crate::storage::TextSearchDictionaryBehavior) -> u8 {
    match behavior {
        crate::storage::TextSearchDictionaryBehavior::Simple { accept: true } => 0,
        crate::storage::TextSearchDictionaryBehavior::Simple { accept: false } => 1,
        crate::storage::TextSearchDictionaryBehavior::EnglishStem => 2,
    }
}

struct ManifestTextSearchMappings<'a>(&'a crate::storage::TextSearchMappings);

impl core::fmt::Display for ManifestTextSearchMappings<'_> {
    fn fmt(&self, output: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for token in 0..crate::storage::TEXT_SEARCH_TOKEN_TYPES {
            let count = usize::from(self.0.counts[token]);
            write!(output, " {count}")?;
            for dictionary in &self.0.dictionaries[token][..count] {
                write!(output, " {dictionary}")?;
            }
        }
        Ok(())
    }
}

impl core::fmt::Display for ManifestName<'_> {
    fn fmt(&self, output: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0.is_empty() {
            return output.write_str("-");
        }
        for byte in self.0.as_bytes() {
            write!(output, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl core::fmt::Display for ManifestZeroName<'_> {
    fn fmt(&self, output: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0.is_empty() {
            return output.write_str("0");
        }
        for byte in self.0.as_bytes() {
            write!(output, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl core::fmt::Display for ManifestViewColumns {
    fn fmt(&self, output: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            output,
            "{}:{}:",
            u8::from(self.0.has_aliases()),
            self.0.len()
        )?;
        for (index, name) in self.0.names().iter().enumerate() {
            if index != 0 {
                output.write_str(",")?;
            }
            for byte in name.as_str().as_bytes() {
                write!(output, "{byte:02x}")?;
            }
            let default = self
                .0
                .default_at(index)
                .expect("view column index is bounded by its name slice");
            match default {
                ColumnDefault::None => output.write_str("/0/0/0")?,
                ColumnDefault::Constant { value, expression } => {
                    write!(output, "/1/{}/", default_to_hex(&Some(value)).as_str())?;
                    for byte in expression.as_str().as_bytes() {
                        write!(output, "{byte:02x}")?;
                    }
                }
                ColumnDefault::Expression(expression) => {
                    output.write_str("/2/0/")?;
                    for byte in expression.as_str().as_bytes() {
                        write!(output, "{byte:02x}")?;
                    }
                }
                ColumnDefault::Generated(_) => {
                    return Err(core::fmt::Error);
                }
            }
        }
        Ok(())
    }
}

struct ManifestRoutineResult(crate::storage::RoutineResult);

impl core::fmt::Display for ManifestRoutineResult {
    fn fmt(&self, output: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(output, "{} ", self.0.ctype.code())?;
        match self.0.user_type {
            Some(identity) => write!(
                output,
                "{} {}",
                ManifestName(identity.schema.as_str()),
                ManifestName(identity.storage_name().as_str())
            ),
            None => output.write_str("- -"),
        }
    }
}

impl core::fmt::Display for ManifestDependencies<'_> {
    fn fmt(&self, output: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(output, "{}", self.0.entries().len())?;
        for dependency in self.0.entries() {
            write!(output, " {} ", dependency.class as u8)?;
            write!(output, "{} ", dependency.identity.storage_key())?;
            for byte in dependency.schema.as_str().as_bytes() {
                write!(output, "{byte:02x}")?;
            }
            output.write_str(" ")?;
            for byte in dependency.name.as_str().as_bytes() {
                write!(output, "{byte:02x}")?;
            }
            output.write_str(" ")?;
            for byte in dependency.referenced_schema.as_str().as_bytes() {
                write!(output, "{byte:02x}")?;
            }
            output.write_str(" ")?;
            for byte in dependency.referenced_name.as_str().as_bytes() {
                write!(output, "{byte:02x}")?;
            }
            write!(output, " {}", dependency.referenced_columns)?;
        }
        Ok(())
    }
}

fn parse_stored_query_dependencies(
    words: &mut core::str::Split<'_, char>,
) -> Result<crate::storage::StoredQueryDependencies, CheckpointSetupError> {
    let count: usize = parse_field(words.next(), "stored-query dependency count")?;
    if count > crate::storage::MAX_STORED_QUERY_DEPENDENCIES {
        return Err(CheckpointSetupError::Corrupt(
            "too many stored-query dependencies",
        ));
    }
    let mut dependencies =
        crate::storage::StoredQueryDependencies::with_recovery_limit(count.max(1));
    for _ in 0..count {
        let code: u8 = parse_field(words.next(), "stored-query dependency class")?;
        let class = crate::storage::DependencyClass::from_code(code).ok_or(
            CheckpointSetupError::Corrupt("unknown stored-query dependency class"),
        )?;
        let identity = crate::storage::StoredDependencyIdentity::from_storage(
            class,
            words.next().ok_or(CheckpointSetupError::Corrupt(
                "stored-query dependency identity missing",
            ))?,
        )
        .ok_or(CheckpointSetupError::Corrupt(
            "invalid stored-query dependency identity",
        ))?;
        let schema = decode_hex_name(words.next().ok_or(CheckpointSetupError::Corrupt(
            "stored-query dependency schema missing",
        ))?)?;
        let name = decode_hex_name(words.next().ok_or(CheckpointSetupError::Corrupt(
            "stored-query dependency name missing",
        ))?)?;
        let referenced_schema = decode_hex_name(words.next().ok_or(
            CheckpointSetupError::Corrupt("stored-query referenced schema missing"),
        )?)?;
        let referenced_name = decode_hex_name(words.next().ok_or(
            CheckpointSetupError::Corrupt("stored-query referenced name missing"),
        )?)?;
        let referenced_columns = parse_field(words.next(), "stored-query referenced columns")?;
        dependencies
            .serialized_push(SerializedStoredQueryDependency {
                class,
                identity,
                schema: sql_name(&schema)?,
                name: sql_name(&name)?,
                referenced_schema: sql_name(&referenced_schema)?,
                referenced_name: sql_name(&referenced_name)?,
                referenced_columns,
            })
            .map_err(|_| CheckpointSetupError::Corrupt("too many stored-query dependencies"))?;
    }
    Ok(dependencies)
}

fn sql_name(s: &str) -> Result<SqlName, CheckpointSetupError> {
    SqlName::parse(s).map_err(|_| CheckpointSetupError::Corrupt("name too long in manifest"))
}

fn manifest_routine_result(
    words: &mut core::str::Split<'_, char>,
) -> Result<crate::storage::RoutineResult, CheckpointSetupError> {
    let code: u8 = parse_field(words.next(), "catalog type code")?;
    let ctype = ColType::from_code(code)
        .ok_or(CheckpointSetupError::Corrupt("invalid catalog type code"))?;
    let schema = words
        .next()
        .ok_or(CheckpointSetupError::Corrupt("catalog type schema missing"))?;
    let name = words
        .next()
        .ok_or(CheckpointSetupError::Corrupt("catalog type name missing"))?;
    let user_type = match (schema, name) {
        ("-", "-") => None,
        ("-", _) | (_, "-") => {
            return Err(CheckpointSetupError::Corrupt(
                "partial catalog type identity",
            ));
        }
        _ => Some(
            crate::storage::UserTypeName::from_storage(
                sql_name(&decode_hex_name(schema)?)?,
                &decode_hex_name(name)?,
            )
            .map_err(|_| CheckpointSetupError::Corrupt("invalid stored type identity"))?,
        ),
    };
    Ok(crate::storage::RoutineResult { ctype, user_type })
}

fn empty_column() -> ColumnMeta {
    ColumnMeta {
        name: SqlName::parse("").expect("empty fits"),
        ctype: ColType::Bool,
        type_mod: -1,
        collation: crate::sql::ast::Collation::None,
        storage: crate::sql::ast::ColumnStorage::Plain,
        compression: crate::sql::ast::ColumnCompression::Default,
        not_null: crate::storage::NotNullOrigin::Nullable,
        not_null_inheritable: true,
        unique: false,
        primary: false,
        auto_increment: false,
        default: ColumnDefault::NONE,
        is_identity: false,
        identity_always: false,
        auto_increment_step: 1,
        user_type: None,
        statistics_target: -1,
        statistics_options: crate::sql::ast::ColumnStatisticsOptions::DEFAULT,
    }
}

/// Column defaults travel in the manifest as hex of the WAL default
/// encoding ("-" for none-with-no-bytes readability).
fn default_to_hex(d: &Option<OwnedDatum>) -> StackStr<{ 2 * crate::wal::MAX_DEFAULT_ENCODED }> {
    let mut scratch = [0u8; crate::wal::MAX_DEFAULT_ENCODED];
    let n = crate::wal::encode_default_bytes(d, &mut scratch);
    let mut out = StackStr::<{ 2 * crate::wal::MAX_DEFAULT_ENCODED }>::new();
    use core::fmt::Write;
    for b in &scratch[..n] {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn default_from_hex(hex: &str) -> Result<Option<OwnedDatum>, CheckpointSetupError> {
    let corrupt = || CheckpointSetupError::Corrupt("bad default encoding");
    if !hex.len().is_multiple_of(2) || hex.len() > 2 * crate::wal::MAX_DEFAULT_ENCODED {
        return Err(corrupt());
    }
    let mut bytes = [0u8; crate::wal::MAX_DEFAULT_ENCODED];
    let n = hex.len() / 2;
    for i in 0..n {
        bytes[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| corrupt())?;
    }
    let mut at = 0usize;
    let d = crate::wal::decode_default(&bytes[..n], &mut at).ok_or_else(corrupt)?;
    if at != n {
        return Err(corrupt());
    }
    Ok(d)
}

fn write_partition_manifest(
    buffer: &mut FixedBuf,
    partition: PartitionDef,
) -> Result<(), SqlError> {
    let mut line = StackStr::<4096>::new();
    use core::fmt::Write;
    let _ = write!(line, "part 5");
    if let Some(scheme) = partition.scheme {
        let strategy = match scheme.strategy {
            PartitionStrategy::Range => "r",
            PartitionStrategy::List => "l",
            PartitionStrategy::Hash => "h",
        };
        let _ = write!(line, " p {strategy} {}", scheme.n_keys);
        for key in &scheme.keys[..usize::from(scheme.n_keys)] {
            let _ = write!(line, " {key}");
        }
    } else {
        let _ = write!(line, " n");
    }
    if let Some(attachment) = partition.attachment {
        let state = match attachment.state {
            crate::storage::PartitionAttachmentState::Attached => "a",
            crate::storage::PartitionAttachmentState::DetachPending => "d",
        };
        let _ = write!(line, " c {} {state}", attachment.parent);
        match attachment.bound {
            PartitionBound::Default => {
                let _ = write!(line, " d");
            }
            PartitionBound::Hash { modulus, remainder } => {
                let _ = write!(line, " h {modulus} {remainder}");
            }
            PartitionBound::List { values, n_values } => {
                let _ = write!(line, " l {n_values}");
                for value in &values[..usize::from(n_values)] {
                    let _ = write!(line, " {}", default_to_hex(&Some(*value)).as_str());
                }
            }
            PartitionBound::Range {
                lower,
                upper,
                n_keys,
            } => {
                let _ = write!(line, " r {n_keys}");
                for i in 0..usize::from(n_keys) {
                    let _ = write!(
                        line,
                        " {} {}",
                        partition_bound_value_text(lower[i]).as_str(),
                        partition_bound_value_text(upper[i]).as_str()
                    );
                }
            }
        }
    } else {
        let _ = write!(line, " n");
    }
    if let Some(detached) = partition.detached_bound {
        let strategy = match detached.scheme.strategy {
            PartitionStrategy::Range => "r",
            PartitionStrategy::List => "l",
            PartitionStrategy::Hash => "h",
        };
        let mut name_hex = StackStr::<130>::new();
        for byte in detached.name.as_str().as_bytes() {
            let _ = write!(name_hex, "{byte:02x}");
        }
        let _ = write!(
            line,
            " d {} {strategy} {}",
            name_hex.as_str(),
            detached.scheme.n_keys
        );
        for key in &detached.scheme.keys[..usize::from(detached.scheme.n_keys)] {
            let _ = write!(line, " {key}");
        }
        write_partition_bound_manifest(&mut line, detached.bound);
    } else {
        let _ = write!(line, " n");
    }
    write_manifest(buffer, line.as_str())
}

fn write_partition_bound_manifest(line: &mut StackStr<4096>, bound: PartitionBound) {
    use core::fmt::Write;
    match bound {
        PartitionBound::Default => {
            let _ = write!(line, " d");
        }
        PartitionBound::Hash { modulus, remainder } => {
            let _ = write!(line, " h {modulus} {remainder}");
        }
        PartitionBound::List { values, n_values } => {
            let _ = write!(line, " l {n_values}");
            for value in &values[..usize::from(n_values)] {
                let _ = write!(line, " {}", default_to_hex(&Some(*value)).as_str());
            }
        }
        PartitionBound::Range {
            lower,
            upper,
            n_keys,
        } => {
            let _ = write!(line, " r {n_keys}");
            for i in 0..usize::from(n_keys) {
                let _ = write!(
                    line,
                    " {} {}",
                    partition_bound_value_text(lower[i]).as_str(),
                    partition_bound_value_text(upper[i]).as_str()
                );
            }
        }
    }
}

fn partition_bound_value_text(
    value: PartitionBoundValue,
) -> StackStr<{ 2 * crate::wal::MAX_DEFAULT_ENCODED }> {
    match value {
        PartitionBoundValue::MinValue => StackStr::from_str("min"),
        PartitionBoundValue::MaxValue => StackStr::from_str("max"),
        PartitionBoundValue::Value(value) => default_to_hex(&Some(value)),
    }
}

fn partition_bound_value_from_text(
    text: &str,
) -> Result<PartitionBoundValue, CheckpointSetupError> {
    match text {
        "min" => Ok(PartitionBoundValue::MinValue),
        "max" => Ok(PartitionBoundValue::MaxValue),
        _ => Ok(PartitionBoundValue::Value(
            default_from_hex(text)?.ok_or(CheckpointSetupError::Corrupt("bad partition bound"))?,
        )),
    }
}

fn parse_partition_manifest(
    words: &mut core::str::Split<'_, char>,
) -> Result<PartitionDef, CheckpointSetupError> {
    let corrupt = || CheckpointSetupError::Corrupt("bad partition metadata");
    if words.next().ok_or_else(corrupt)? != "5" {
        return Err(corrupt());
    }
    let scheme = match words.next().ok_or_else(corrupt)? {
        "n" => None,
        "p" => {
            let strategy = match words.next().ok_or_else(corrupt)? {
                "r" => PartitionStrategy::Range,
                "l" => PartitionStrategy::List,
                "h" => PartitionStrategy::Hash,
                _ => return Err(corrupt()),
            };
            let n_keys: u8 = parse_field(words.next(), "partition key count")?;
            if usize::from(n_keys) > crate::storage::MAX_PARTITION_KEYS {
                return Err(corrupt());
            }
            let mut keys = [0u16; crate::storage::MAX_PARTITION_KEYS];
            for key in &mut keys[..usize::from(n_keys)] {
                *key = parse_field(words.next(), "partition key")?;
            }
            Some(crate::storage::PartitionScheme {
                strategy,
                keys,
                n_keys,
            })
        }
        _ => return Err(corrupt()),
    };
    let attachment = match words.next().ok_or_else(corrupt)? {
        "n" => None,
        "c" => {
            let parent: u16 = parse_field(words.next(), "partition parent")?;
            let state = match words.next().ok_or_else(corrupt)? {
                "a" => crate::storage::PartitionAttachmentState::Attached,
                "d" => crate::storage::PartitionAttachmentState::DetachPending,
                _ => return Err(corrupt()),
            };
            let bound = match words.next().ok_or_else(corrupt)? {
                "d" => PartitionBound::Default,
                "h" => PartitionBound::Hash {
                    modulus: parse_field(words.next(), "partition modulus")?,
                    remainder: parse_field(words.next(), "partition remainder")?,
                },
                "l" => {
                    let n_values: u8 = parse_field(words.next(), "partition list count")?;
                    if usize::from(n_values) > crate::storage::MAX_PARTITION_LIST_VALUES {
                        return Err(corrupt());
                    }
                    let mut values = [OwnedDatum::Null; crate::storage::MAX_PARTITION_LIST_VALUES];
                    for value in &mut values[..usize::from(n_values)] {
                        *value = default_from_hex(words.next().ok_or_else(corrupt)?)?
                            .ok_or_else(corrupt)?;
                    }
                    PartitionBound::List { values, n_values }
                }
                "r" => {
                    let n_keys: u8 = parse_field(words.next(), "partition key count")?;
                    if usize::from(n_keys) > crate::storage::MAX_PARTITION_KEYS {
                        return Err(corrupt());
                    }
                    let mut lower =
                        [PartitionBoundValue::MinValue; crate::storage::MAX_PARTITION_KEYS];
                    let mut upper = lower;
                    for i in 0..usize::from(n_keys) {
                        lower[i] =
                            partition_bound_value_from_text(words.next().ok_or_else(corrupt)?)?;
                        upper[i] =
                            partition_bound_value_from_text(words.next().ok_or_else(corrupt)?)?;
                    }
                    PartitionBound::Range {
                        lower,
                        upper,
                        n_keys,
                    }
                }
                _ => return Err(corrupt()),
            };
            Some(crate::storage::PartitionAttachment {
                parent,
                bound,
                state,
            })
        }
        _ => return Err(corrupt()),
    };
    let detached_bound = match words.next().ok_or_else(corrupt)? {
        "n" => None,
        "d" => {
            let name = sql_name(&decode_hex_name(words.next().ok_or_else(corrupt)?)?)?;
            let strategy = match words.next().ok_or_else(corrupt)? {
                "r" => PartitionStrategy::Range,
                "l" => PartitionStrategy::List,
                "h" => PartitionStrategy::Hash,
                _ => return Err(corrupt()),
            };
            let n_keys: u8 = parse_field(words.next(), "detached partition key count")?;
            if usize::from(n_keys) > crate::storage::MAX_PARTITION_KEYS {
                return Err(corrupt());
            }
            let mut keys = [0u16; crate::storage::MAX_PARTITION_KEYS];
            for key in &mut keys[..usize::from(n_keys)] {
                *key = parse_field(words.next(), "detached partition key")?;
            }
            Some(crate::storage::DetachedPartitionBound {
                identity: None,
                name,
                scheme: crate::storage::PartitionScheme {
                    strategy,
                    keys,
                    n_keys,
                },
                bound: parse_partition_bound_manifest(words)?,
            })
        }
        _ => return Err(corrupt()),
    };
    Ok(PartitionDef {
        scheme,
        attachment,
        detached_bound,
    })
}

fn parse_partition_bound_manifest(
    words: &mut core::str::Split<'_, char>,
) -> Result<PartitionBound, CheckpointSetupError> {
    let corrupt = || CheckpointSetupError::Corrupt("bad partition bound");
    match words.next().ok_or_else(corrupt)? {
        "d" => Ok(PartitionBound::Default),
        "h" => Ok(PartitionBound::Hash {
            modulus: parse_field(words.next(), "partition modulus")?,
            remainder: parse_field(words.next(), "partition remainder")?,
        }),
        "l" => {
            let n_values: u8 = parse_field(words.next(), "partition list count")?;
            if usize::from(n_values) > crate::storage::MAX_PARTITION_LIST_VALUES {
                return Err(corrupt());
            }
            let mut values = [OwnedDatum::Null; crate::storage::MAX_PARTITION_LIST_VALUES];
            for value in &mut values[..usize::from(n_values)] {
                *value =
                    default_from_hex(words.next().ok_or_else(corrupt)?)?.ok_or_else(corrupt)?;
            }
            Ok(PartitionBound::List { values, n_values })
        }
        "r" => {
            let n_keys: u8 = parse_field(words.next(), "partition key count")?;
            if usize::from(n_keys) > crate::storage::MAX_PARTITION_KEYS {
                return Err(corrupt());
            }
            let mut lower = [PartitionBoundValue::MinValue; crate::storage::MAX_PARTITION_KEYS];
            let mut upper = lower;
            for i in 0..usize::from(n_keys) {
                lower[i] = partition_bound_value_from_text(words.next().ok_or_else(corrupt)?)?;
                upper[i] = partition_bound_value_from_text(words.next().ok_or_else(corrupt)?)?;
            }
            Ok(PartitionBound::Range {
                lower,
                upper,
                n_keys,
            })
        }
        _ => Err(corrupt()),
    }
}

#[cfg(test)]
mod stored_dependency_tests {
    #[test]
    fn manifest_hex_decode_rejects_non_ascii_without_panicking() {
        for invalid in ["é", "🦀", "0é0", "0g", "abc"] {
            assert!(super::decode_manifest_hex_bytes(invalid).is_err());
        }
        assert_eq!(super::decode_manifest_hex_bytes("fe00").unwrap(), [254, 0]);
        assert!(super::decode_hex_name("ff").is_err());
        assert_eq!(super::decode_hex_name("c3a9").unwrap(), "é");
        assert_eq!(super::decode_manifest_hex_bytes("-").unwrap(), []);
    }

    use super::*;
    use crate::mem::budget::Budget;
    use crate::mem::buffer::FixedBuf;
    use crate::storage::{DependencyClass, StoredDependencyIdentity, StoredQueryDependencies};

    #[test]
    fn index_file_owners_keep_set_counts_shared_blocks_once_at_exact_capacity() {
        let mut budget = Budget::new(FixedMap::<BlockId, ()>::budget_bytes(2));
        let mut seen = FixedMap::new(&mut budget, "test block membership", 2).unwrap();
        let mut blocks = Vec::with_capacity(2);
        crate::mem::guard::forbid_alloc(|| {
            {
                let mut keep = BlockKeepSet::new(&mut blocks, &mut seen);
                for _ in 0..128 {
                    for tag in [1, 2, 1, 2] {
                        retain_block(&mut keep, BlockId([tag; 32])).unwrap();
                    }
                }
                let error = keep.insert(BlockId([3; 32])).unwrap_err();
                assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
                assert_eq!(keep.blocks.len(), 2);
                assert_eq!(keep.seen.len(), 2);
                assert_eq!(keep.blocks[0].0, BlockId([1; 32]));
                assert_eq!(keep.blocks[1].0, BlockId([2; 32]));
                keep.insert(BlockId([1; 32])).unwrap();
            }
            let mut retry = BlockKeepSet::new(&mut blocks, &mut seen);
            retry.insert(BlockId([3; 32])).unwrap();
            assert_eq!(retry.blocks.len(), 1);
            assert_eq!(retry.seen.len(), 1);
        });
    }

    fn index_gc_fixture(label: &str, capacity: usize) -> (Checkpointer, Storage, usize, Config) {
        let mut config = Config::default_dev();
        config.object_store_on = true;
        config.object_store_sim = true;
        config.block_cache_bytes = 0;
        config.disk_cache_bytes = 0;
        config.memtable_bytes = 1 << 16;
        config.max_tables = 2;
        config.max_connections = 2;
        config.table_rows = 16;
        config.txn_rows = 16;
        config.max_value_indexes = 2;
        config.value_index_rows = 4;
        config.checkpoint_live_blocks = capacity;
        config.object_store_bucket = format!("{label}-{}", std::process::id());
        config.data_dir = std::env::temp_dir()
            .join(&config.object_store_bucket)
            .to_str()
            .unwrap()
            .to_owned();
        config.remove_test_data_dir_on_drop();
        let mut budget = Budget::new(
            config.memtable_bytes
                + Storage::extra_budget_bytes(&config)
                + Checkpointer::budget_bytes(&config)
                + crate::storage::SpillReader::budget_bytes(true, 3),
        );
        let checkpointer = Checkpointer::new(&config, &mut budget).unwrap();
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let reader = crate::storage::SpillReader::new(
            &mut budget,
            3,
            Some(checkpointer.block_stack()),
            None,
        )
        .unwrap();
        storage.attach_spill(reader);
        let mut definition = TableDef {
            schema: SqlName::parse("public").unwrap(),
            name: SqlName::parse("saved_index").unwrap(),
            n_columns: 1,
            ..TableDef::empty()
        };
        definition.columns[0] = ColumnMeta {
            name: SqlName::parse("id").unwrap(),
            ctype: ColType::Int4,
            unique: true,
            ..ColumnMeta::EMPTY
        };
        let table = storage.create_table(definition).unwrap();
        (checkpointer, storage, table, config)
    }

    #[test]
    fn parallel_physical_manifest_retry_preserves_bytes_while_backup_uses_scratch() {
        use std::time::{Duration, Instant};
        let (mut checkpoint, mut storage, _table, config) =
            index_gc_fixture("manifest-backup-retry", 16);
        assert!(checkpoint.checkpoint(&mut storage).unwrap().is_some());
        checkpoint.finish_maintenance(&storage).unwrap();
        storage.set_lsn(7);
        for _ in 0..64 {
            if checkpoint.manifest_publication_pending() {
                break;
            }
            assert!(matches!(
                checkpoint.checkpoint_step(&mut storage).unwrap(),
                super::CheckpointStep::Working
            ));
        }
        assert!(checkpoint.manifest_publication_pending());
        let captured = checkpoint.manifest_output.readable().to_vec();
        let namespace = crate::object_store::sim::open_namespace(&config.object_store_bucket, 274);
        namespace.borrow_mut().faults.transient_per_mille = 1000;
        let now = Instant::now();
        for turn in 0..16 {
            checkpoint
                .manifest_uploads
                .advance(now + Duration::from_millis(turn * 250));
            if checkpoint.manifest_uploads.has_completion() {
                break;
            }
        }
        let error = checkpoint
            .advance_manifest_publication(&mut storage)
            .err()
            .expect("exhausted request attempts must retain a root retry");
        assert_eq!(error.sqlstate, "58030");
        assert!(matches!(
            checkpoint.manifest_publication,
            Some(super::ManifestPublication::Retry { lsn: 7, .. })
        ));
        namespace.borrow_mut().faults.transient_per_mille = 0;
        assert_eq!(checkpoint.create_backup("during-retry").unwrap(), 0);
        assert_ne!(checkpoint.manifest_buf.readable(), captured);
        assert_eq!(checkpoint.manifest_output.readable(), captured);
        checkpoint.manifest_publication = Some(super::ManifestPublication::Retry {
            lsn: 7,
            at: Instant::now(),
        });
        assert_eq!(checkpoint.checkpoint(&mut storage).unwrap(), Some(7));
        let key = format!("{}manifest", config.object_store_prefix);
        assert_eq!(namespace.borrow().object_bytes(&key).unwrap(), captured);
        assert!(namespace.borrow().blind_overwrites.is_empty());
        drop(storage);
        drop(checkpoint);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }

    #[test]
    fn parallel_credentials_renewal_requires_read_and_write_before_installing() {
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        fn request(stream: &mut TcpStream) -> (String, Vec<u8>) {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut part = [0u8; 4096];
            let end = loop {
                let n = stream.read(&mut part).unwrap();
                assert_ne!(n, 0);
                bytes.extend_from_slice(&part[..n]);
                if let Some(at) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    break at + 4;
                }
            };
            let head = core::str::from_utf8(&bytes[..end]).unwrap().to_owned();
            assert!(head.contains("authorization: AWS4-HMAC-SHA256 "));
            assert!(head.contains("Credential=candidate/"));
            let length = if head.starts_with("PUT ") {
                head.lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap()
                    .parse::<usize>()
                    .unwrap()
            } else {
                assert!(head.starts_with("GET "));
                0
            };
            while bytes.len() < end + length {
                let n = stream.read(&mut part).unwrap();
                assert_ne!(n, 0);
                bytes.extend_from_slice(&part[..n]);
            }
            (head, bytes[end..].to_vec())
        }

        fn reply(stream: &mut TcpStream, status: &str, body: &[u8]) {
            write!(stream,
                "HTTP/1.1 {status}\r\nETag: \"candidate\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()).unwrap();
            stream.write_all(body).unwrap();
        }

        for case in [
            "read-only",
            "write-only",
            "both",
            "replaced",
            "ambiguous",
            "conditional-denied",
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut config =
                crate::object_store::upload::tests::config(listener.local_addr().unwrap().port());
            config.object_store_bucket = "credential-renewal".to_owned();
            config.object_store_credentials_file = "configured-credential-file".to_owned();
            config.data_dir = std::env::temp_dir()
                .join(format!("credential-renewal-{case}-{}", std::process::id()))
                .to_string_lossy()
                .into_owned();
            config.max_tables = 2;
            config.max_connections = 2;
            config.table_rows = 16;
            config.max_value_indexes = 2;
            config.block_cache_bytes = 0;
            config.disk_cache_bytes = 0;
            let (finished, done) = mpsc::channel();
            let server = std::thread::spawn(move || {
                let (mut put, _) = listener.accept().unwrap();
                let (head, body) = request(&mut put);
                assert!(head.starts_with("PUT "));
                assert!(head.contains("if-match: \"current\"\r\n"));
                assert_eq!(
                    body,
                    writer_fence_record("active", WriterToken(1))
                        .as_str()
                        .as_bytes()
                );
                if case == "read-only" {
                    reply(&mut put, "403 Forbidden", b"");
                    listener.set_nonblocking(true).unwrap();
                    let deadline = Instant::now() + Duration::from_secs(5);
                    let mut reads = 0;
                    loop {
                        assert!(
                            Instant::now() < deadline,
                            "credential request did not finish"
                        );
                        match listener.accept() {
                            Ok((mut get, _)) => {
                                assert!(request(&mut get).0.starts_with("GET "));
                                let record = writer_fence_record("active", WriterToken(1));
                                reply(&mut get, "200 OK", record.as_str().as_bytes());
                                reads += 1;
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                            Err(error) => panic!("accept verification: {error}"),
                        }
                        match done.try_recv() {
                            Ok(()) | Err(mpsc::TryRecvError::Disconnected) => return reads,
                            Err(mpsc::TryRecvError::Empty) => std::thread::yield_now(),
                        }
                    }
                }
                reply(
                    &mut put,
                    if matches!(case, "ambiguous" | "conditional-denied") {
                        "412 Precondition Failed"
                    } else {
                        "200 OK"
                    },
                    b"",
                );
                let (mut get, _) = listener.accept().unwrap();
                assert!(request(&mut get).0.starts_with("GET "));
                if case == "write-only" {
                    reply(&mut get, "403 Forbidden", b"");
                } else {
                    let record = writer_fence_record(
                        "active",
                        WriterToken(if case == "replaced" { 2 } else { 1 }),
                    );
                    reply(&mut get, "200 OK", record.as_str().as_bytes());
                }
                if matches!(case, "ambiguous" | "conditional-denied") {
                    let (mut retry, _) = listener.accept().unwrap();
                    let (head, retried_body) = request(&mut retry);
                    assert!(head.starts_with("PUT "));
                    assert!(head.contains("if-match: \"candidate\"\r\n"));
                    assert_eq!(retried_body, body);
                    if case == "conditional-denied" {
                        reply(&mut retry, "403 Forbidden", b"");
                        return 1;
                    }
                    reply(&mut retry, "200 OK", b"");
                    let (mut verified, _) = listener.accept().unwrap();
                    assert!(request(&mut verified).0.starts_with("GET "));
                    reply(&mut verified, "200 OK", &body);
                    return 2;
                }
                1
            });
            let mut budget = Budget::new(Checkpointer::budget_bytes(&config));
            let mut checkpoint = Checkpointer::new(&config, &mut budget).unwrap();
            checkpoint.writer_token = WriterToken(1);
            checkpoint.writer_fence_etag = Some(EntityTag::parse("\"current\"").unwrap());
            let installed = checkpoint.client.credentials().unwrap();
            let candidate =
                crate::object_store::Credentials::new("candidate", "candidate-secret", "").unwrap();
            let result = crate::mem::guard::forbid_alloc(|| {
                checkpoint.begin_credential_reload(candidate).unwrap();
                assert!(checkpoint.client.credentials() == Some(installed));
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    assert!(
                        Instant::now() < deadline,
                        "credential validation did not finish"
                    );
                    checkpoint.advance_background_io(Instant::now());
                    match checkpoint.advance_credential_reload() {
                        Ok(CredentialReloadStep::Pending) => {
                            assert!(
                                checkpoint.client.credentials() == Some(installed),
                                "candidate remains private until read and write validation finish"
                            );
                        }
                        result => break result,
                    }
                    std::thread::yield_now();
                }
            });
            let _ = finished.send(());
            let reads = server.join().unwrap();
            let accepted = matches!(result, Ok(CredentialReloadStep::Installed));
            assert_eq!(accepted, matches!(case, "both" | "ambiguous"), "{case}");
            assert_eq!(
                reads,
                usize::from(case != "read-only") + usize::from(case == "ambiguous"),
                "a rejected write cannot become successful through a read fallback"
            );
            assert_eq!(checkpoint.writer_was_fenced(), case == "replaced");
            assert!(
                checkpoint.client.credentials()
                    == Some(if accepted { candidate } else { installed })
            );
            assert!(checkpoint.credential_renewal.is_none());
            assert!(checkpoint.block_uploads.admits_requests());
            drop(checkpoint);
            std::fs::remove_dir_all(&config.data_dir).unwrap();
        }
    }

    fn write_gc_index(checkpointer: &Checkpointer, payload: &[u8]) -> ValueIndexHandle {
        let mut writer = ValueIndexWriter::new();
        let mut blocks = checkpointer.blocks.lock().unwrap();
        writer
            .append_covering(
                &mut *blocks,
                (11, 1, 7),
                b"key",
                payload,
                &mut |left, right| left.cmp(right),
            )
            .unwrap();
        writer.finish(&mut *blocks, 7).unwrap().unwrap()
    }

    fn index_gc_hex(id: BlockId) -> String {
        let mut key = [0; 64];
        id.write_key(&mut key);
        core::str::from_utf8(&key).unwrap().to_owned()
    }

    #[test]
    fn parallel_physical_completed_merge_survives_cleanup_before_its_publication() {
        for finish_before_cleanup in [false, true] {
            let label = if finish_before_cleanup {
                "merge-completed-gc"
            } else {
                "merge-finishing-gc"
            };
            let (mut checkpoint, mut storage, table, config) = index_gc_fixture(label, 32);
            let mut saved = [None; 4];
            for (index, value) in [b"first".as_slice(), b"second", b"merged", b"orphan"]
                .into_iter()
                .enumerate()
            {
                checkpoint.slice_writer.reset();
                let mut blocks = checkpoint.blocks.lock().unwrap();
                checkpoint
                    .slice_writer
                    .append(&mut *blocks, 1, value)
                    .unwrap();
                saved[index] = Some(PrevSst {
                    handle: checkpoint
                        .slice_writer
                        .finish(&mut *blocks)
                        .unwrap()
                        .unwrap(),
                    count: 1,
                    crc: 0,
                });
            }
            let [Some(old0), Some(old1), Some(merged), Some(orphan)] = saved else {
                unreachable!()
            };
            assert!(checkpoint.prev_ssts[table].push(old0));
            assert!(checkpoint.prev_ssts[table].push(old1));
            checkpoint.merge_job = Some(MergeJob {
                slot: table,
                at: 0,
                old0,
                old1,
                drop_tombstones: true,
                oldest_snapshot: None,
                phase: MergePhase::Finish {
                    merged: Some(merged),
                },
                schedule_len: 0,
                count: 1,
                crc: Crc32c::new(),
            });
            checkpoint.published_lsn_pending_maintenance = Some(0);
            checkpoint.block_garbage_pending = true;
            if finish_before_cleanup {
                assert!(matches!(
                    checkpoint.checkpoint_step(&mut storage).unwrap(),
                    CheckpointStep::Working
                ));
            }
            for _ in 0..16 {
                if !checkpoint.block_garbage_pending {
                    break;
                }
                checkpoint.block_garbage_pending = !crate::mem::guard::forbid_alloc(|| {
                    checkpoint.collect_block_garbage_batch(&storage)
                })
                .unwrap();
            }
            assert!(!checkpoint.block_garbage_pending, "cleanup must drain");
            let namespace =
                crate::object_store::sim::open_namespace(&config.object_store_bucket, 0);
            let merged_key = format!(
                "{}blocks/{}",
                config.object_store_prefix,
                index_gc_hex(merged.handle.roster)
            );
            let orphan_key = format!(
                "{}blocks/{}",
                config.object_store_prefix,
                index_gc_hex(orphan.handle.roster)
            );
            assert!(namespace.borrow().object_bytes(&merged_key).is_some());
            assert!(namespace.borrow().object_bytes(&orphan_key).is_none());
            if !finish_before_cleanup {
                checkpoint.merge_beat(&storage).unwrap();
            }
            assert!(checkpoint.merge_job.is_none());
            assert!(checkpoint.merge_done[table].is_some());
            // Publish the merged identity after the earlier sweep. Its graph
            // must still be readable with both caches disabled.
            checkpoint.prev_ssts[table].replace_pair(0, Some(merged));
            checkpoint.merge_done[table] = None;
            assert!(checkpoint.collect_block_garbage_batch(&storage).unwrap());
            let mut scratch = [0u8; crate::store::MAX_PAYLOAD];
            let (len, _) = checkpoint
                .blocks
                .lock()
                .unwrap()
                .get(&merged.handle.roster, &mut scratch)
                .unwrap();
            for bytes in scratch[..len].as_chunks::<32>().0 {
                let id = BlockId(*bytes);
                assert!(
                    namespace
                        .borrow()
                        .object_bytes(&format!(
                            "{}blocks/{}",
                            config.object_store_prefix,
                            index_gc_hex(id)
                        ))
                        .is_some()
                );
            }
            checkpoint.prev_ssts[table].clear();
            assert!(checkpoint.collect_block_garbage_batch(&storage).unwrap());
            assert!(namespace.borrow().object_bytes(&merged_key).is_none());
            drop(storage);
            drop(checkpoint);
            crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        }
    }

    #[test]
    fn index_file_owners_garbage_collection_retains_old_graph_until_last_owner_and_backup() {
        let (mut checkpoint, mut storage, table, config) =
            index_gc_fixture("index-gc-retention", 4);
        let old = write_gc_index(&checkpoint, b"old");
        let new = write_gc_index(&checkpoint, b"new");
        storage
            .install_value_binding(
                table,
                &[0],
                None,
                crate::storage::ColumnSet::EMPTY,
                Some(old),
                crate::storage::ValueIndexInstallation::Recovery,
            )
            .unwrap();
        let first = storage.retain_statement_snapshot(71, |_| 7).unwrap();
        let second = storage.retain_statement_snapshot(72, |_| 7).unwrap();
        for owner in [&first, &second] {
            let scope = owner.enter_files(&storage).unwrap();
            crate::mem::guard::forbid_alloc(|| {
                assert!(
                    storage
                        .walk_value_index(table, &[0], |_, _| Ok(()))
                        .unwrap()
                );
            });
            drop(scope);
        }
        storage
            .install_value_binding(
                table,
                &[0],
                None,
                crate::storage::ColumnSet::EMPTY,
                Some(new),
                crate::storage::ValueIndexInstallation::Recovery,
            )
            .unwrap();
        let manifest = format!(
            "pos3ql-manifest-v34\nlsn 7\nnext_rowid 2\nlatest_transaction_id 1\nnext_lo_oid -\ncatalog_sequence 0\nrole_sequence 16384\nwriter 00000000000000000000000000000001\nvix 0 1 0 0 - {} 1 7\ntypes_end\nend\n",
            index_gc_hex(old.roster)
        );
        // Two backups and two readers share the same graph at the four-block limit.
        for name in ["first", "second"] {
            checkpoint
                .put_immutable(&format!("backups/{name}/manifest"), manifest.as_bytes())
                .unwrap();
        }
        assert!(
            crate::mem::guard::forbid_alloc(|| checkpoint.collect_block_garbage_batch(&storage))
                .unwrap()
        );
        assert_eq!(checkpoint.roster_scratch.len(), 4);
        let namespace = crate::object_store::sim::open_namespace(&config.object_store_bucket, 0);
        let old_key = format!(
            "{}blocks/{}",
            config.object_store_prefix,
            index_gc_hex(old.roster)
        );
        let new_key = format!(
            "{}blocks/{}",
            config.object_store_prefix,
            index_gc_hex(new.roster)
        );
        drop(first);
        assert!(!storage.take_retained_file_reclamation());
        drop(second);
        assert!(storage.take_retained_file_reclamation());
        assert!(checkpoint.collect_block_garbage_batch(&storage).unwrap());
        assert!(namespace.borrow().object_bytes(&old_key).is_some());
        assert!(checkpoint.delete_backup("first").unwrap());
        assert!(checkpoint.collect_block_garbage_batch(&storage).unwrap());
        assert!(namespace.borrow().object_bytes(&old_key).is_some());
        assert!(checkpoint.delete_backup("second").unwrap());
        assert!(checkpoint.collect_block_garbage_batch(&storage).unwrap());
        assert!(namespace.borrow().object_bytes(&old_key).is_none());
        assert!(namespace.borrow().object_bytes(&new_key).is_some());
        drop(storage);
        drop(checkpoint);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }

    #[test]
    fn index_file_owners_garbage_collection_errors_precede_any_deletion() {
        let (mut checkpoint, mut storage, table, config) = index_gc_fixture("index-gc-errors", 1);
        let live = write_gc_index(&checkpoint, b"live");
        let orphan = write_gc_index(&checkpoint, b"orphan");
        storage
            .install_value_binding(
                table,
                &[0],
                None,
                crate::storage::ColumnSet::EMPTY,
                Some(live),
                crate::storage::ValueIndexInstallation::Recovery,
            )
            .unwrap();
        let namespace = crate::object_store::sim::open_namespace(&config.object_store_bucket, 0);
        let orphan_key = format!(
            "{}blocks/{}",
            config.object_store_prefix,
            index_gc_hex(orphan.roster)
        );
        let error = checkpoint
            .collect_block_garbage_batch(&storage)
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(!checkpoint.block_garbage_loaded);
        assert!(checkpoint.doomed_blocks.is_empty());
        assert!(namespace.borrow().object_bytes(&orphan_key).is_some());
        let corrupt = checkpoint
            .blocks
            .lock()
            .unwrap()
            .put(b"bad roster", BlockType::ValueIndexRoster, 0)
            .unwrap();
        storage
            .install_value_binding(
                table,
                &[0],
                None,
                crate::storage::ColumnSet::EMPTY,
                Some(ValueIndexHandle {
                    roster: corrupt,
                    ..live
                }),
                crate::storage::ValueIndexInstallation::Recovery,
            )
            .unwrap();
        let error = checkpoint
            .collect_block_garbage_batch(&storage)
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::IO_ERROR);
        assert!(!checkpoint.block_garbage_loaded);
        assert!(checkpoint.doomed_blocks.is_empty());
        assert!(namespace.borrow().object_bytes(&orphan_key).is_some());
        drop(storage);
        drop(checkpoint);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }

    fn atomic_scope_fixture(name: &str) -> (Checkpointer, Config) {
        let mut config = Config::default_dev();
        config.object_store_on = true;
        config.object_store_sim = true;
        config.object_store_get_slots = 1;
        config.block_cache_bytes = 2 * crate::store::MAX_PAYLOAD;
        config.disk_cache_bytes = 0;
        config.object_store_bucket = format!("{name}-{}", std::process::id());
        config.data_dir = std::env::temp_dir()
            .join(&config.object_store_bucket)
            .to_str()
            .unwrap()
            .to_owned();
        config.remove_test_data_dir_on_drop();
        let mut budget = Budget::new(Checkpointer::budget_bytes(&config) + (4 << 20));
        let checkpointer = Checkpointer::new(&config, &mut budget).unwrap();
        (checkpointer, config)
    }

    #[test]
    fn atomic_block_reads_nested_scope_restores_outer_mode_after_an_error() {
        let (mut checkpointer, config) = atomic_scope_fixture("atomic-scope-nested");
        let blocks = checkpointer.block_stack();
        crate::mem::guard::forbid_alloc(|| {
            let initial = checkpointer.atomic_block_read_scope();
            assert!(!blocks.lock().unwrap().async_gets_enabled());
            drop(initial);
            assert!(!blocks.lock().unwrap().async_gets_enabled());
            checkpointer.enable_async_block_reads();
            let outer = checkpointer.atomic_block_read_scope();
            let fail = || -> Result<(), ()> {
                let _nested = checkpointer.atomic_block_read_scope();
                assert!(!blocks.lock().unwrap().async_gets_enabled());
                Err(())
            };
            assert_eq!(fail(), Err(()));
            assert!(!blocks.lock().unwrap().async_gets_enabled());
            drop(outer);
            assert!(blocks.lock().unwrap().async_gets_enabled());
        });
        drop(checkpointer);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }

    #[test]
    fn atomic_block_reads_deferred_effect_outlives_an_inner_atomic_operation() {
        let (mut checkpointer, config) = atomic_scope_fixture("atomic-scope-deferred");
        checkpointer.enable_async_block_reads();
        let blocks = checkpointer.block_stack();
        crate::mem::guard::forbid_alloc(|| {
            let statement = checkpointer.deferred_atomic_block_read_scope();
            assert!(blocks.lock().unwrap().async_gets_enabled());
            let inner = checkpointer.atomic_block_read_scope();
            statement.activate();
            statement.activate();
            drop(inner);
            assert!(!blocks.lock().unwrap().async_gets_enabled());
            checkpointer.enable_async_block_reads();
            assert!(!blocks.lock().unwrap().async_gets_enabled());
            drop(statement);
            assert!(blocks.lock().unwrap().async_gets_enabled());
        });
        drop(checkpointer);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }

    #[test]
    fn atomic_block_reads_inactive_statement_keeps_async_and_owners_can_drop_out_of_order() {
        let (mut checkpointer, config) = atomic_scope_fixture("atomic-scope-order");
        checkpointer.enable_async_block_reads();
        let blocks = checkpointer.block_stack();
        crate::mem::guard::forbid_alloc(|| {
            drop(checkpointer.deferred_atomic_block_read_scope());
            assert!(blocks.lock().unwrap().async_gets_enabled());
            let first = checkpointer.atomic_block_read_scope();
            let second = checkpointer.atomic_block_read_scope();
            drop(first);
            assert!(!blocks.lock().unwrap().async_gets_enabled());
            drop(second);
            assert!(blocks.lock().unwrap().async_gets_enabled());
        });
        drop(checkpointer);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }

    #[test]
    fn atomic_block_reads_scope_owns_its_controls_after_checkpointer_drop() {
        let (mut checkpointer, config) = atomic_scope_fixture("atomic-scope-owner");
        checkpointer.enable_async_block_reads();
        let blocks = checkpointer.block_stack();
        let scope = crate::mem::guard::forbid_alloc(|| checkpointer.atomic_block_read_scope());
        drop(checkpointer);
        crate::mem::guard::forbid_alloc(|| {
            assert!(!blocks.lock().unwrap().async_gets_enabled());
            drop(scope);
            assert!(blocks.lock().unwrap().async_gets_enabled());
        });
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }

    #[test]
    fn maintenance_metadata_pruning_pages_backups_and_retains_the_oldest_required_commit() {
        let mut config = Config::default_dev();
        config.object_store_on = true;
        config.object_store_sim = true;
        config.block_cache_bytes = 0;
        config.disk_cache_bytes = 0;
        config.object_store_bucket = format!("metadata-prune-pages-{}", std::process::id());
        config.data_dir = std::env::temp_dir()
            .join(&config.object_store_bucket)
            .to_str()
            .unwrap()
            .to_owned();
        config.remove_test_data_dir_on_drop();
        config.max_backups = 70;
        config.checkpoint_garbage_batch_objects = 3;
        config.checkpoint_delete_objects_per_beat = 2;
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let namespace = crate::object_store::sim::open_namespace(&config.object_store_bucket, 0);
        let mut budget = Budget::new(Checkpointer::budget_bytes(&config));
        let mut checkpointer = Checkpointer::new(&config, &mut budget).unwrap();
        let mut batches = Vec::new();
        for first in 1..=9 {
            checkpointer
                .publish_commit_batch(first, b"opaque committed bytes")
                .unwrap();
            batches.push(checkpointer.commit_head.unwrap());
        }
        for index in 0..70 {
            let prepared = if index == 69 {
                "ptx 7 5 50 0 5 10 78\n"
            } else {
                ""
            };
            let manifest = format!(
                "{MANIFEST_HEADER}\nlsn 8\nnext_rowid 2\nlatest_transaction_id 1\nnext_lo_oid -\ncatalog_sequence 0\nrole_sequence 16384\nwriter 00000000000000000000000000000001\n{prepared}types_end\nend\n"
            );
            for (suffix, bytes) in [
                ("manifest", manifest.as_bytes()),
                ("complete", b"receipt".as_slice()),
                ("commit-head", b"head".as_slice()),
            ] {
                checkpointer
                    .client
                    .put(
                        &format!("backups/b{index:02}/{suffix}"),
                        bytes,
                        Precondition::IfNoneMatchAny,
                    )
                    .unwrap();
            }
        }
        let lookalike = format!("commits/{:020}-{}.batch", 0, "a".repeat(80));
        checkpointer
            .client
            .put(
                &lookalike,
                b"not a writer-generated journal name",
                Precondition::IfNoneMatchAny,
            )
            .unwrap();
        assert!(!checkpointer.prune_commit_batch(8).unwrap());
        let cursor = match checkpointer.commit_prune {
            CommitPruneProgress::Backups { cursor } => cursor,
            _ => panic!("backup inventory must span several bounded pages"),
        };
        let names = checkpointer.prune_backup_scratch.len();
        assert!(names > 0 && names < 70);
        namespace.borrow_mut().faults.fail_from_op = Some(0);
        assert_eq!(
            crate::mem::guard::forbid_alloc(|| checkpointer.prune_commit_batch(8))
                .unwrap_err()
                .sqlstate,
            sqlstate::IO_ERROR
        );
        assert_eq!(checkpointer.prune_backup_scratch.len(), names);
        assert!(matches!(checkpointer.commit_prune,
            CommitPruneProgress::Backups { cursor: retained } if retained == cursor));
        namespace.borrow_mut().faults.fail_from_op = None;
        for turn in 0..80 {
            if matches!(
                checkpointer.commit_prune,
                CommitPruneProgress::FindBoundary { floor: 4, .. }
            ) {
                break;
            }
            let before = namespace.borrow().op_count;
            assert!(
                !crate::mem::guard::forbid_alloc(|| checkpointer.prune_commit_batch(8)).unwrap()
            );
            assert!(
                namespace.borrow().op_count - before <= 1,
                "turn {turn} exceeded one metadata read"
            );
        }
        assert_eq!(checkpointer.prune_backup_scratch.len(), 70);
        assert!(matches!(
            checkpointer.commit_prune,
            CommitPruneProgress::FindBoundary { floor: 4, .. }
        ));
        let mut complete = false;
        for _ in 0..80 {
            complete =
                crate::mem::guard::forbid_alloc(|| checkpointer.prune_commit_batch(8)).unwrap();
            if complete {
                break;
            }
        }
        assert!(complete);
        for batch in batches {
            for suffix in ["head", "batch"] {
                let key = format!(
                    "{}commits/{:020}-{:08x}.{suffix}",
                    config.object_store_prefix, batch.first_lsn, batch.digest
                );
                assert_eq!(
                    namespace.borrow().object_bytes(&key).is_some(),
                    batch.first_lsn >= 4
                );
            }
        }
        assert!(
            namespace
                .borrow()
                .object_bytes(&format!("{}{lookalike}", config.object_store_prefix))
                .is_some()
        );
        drop(checkpointer);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }

    #[test]
    fn maintenance_metadata_stalled_journal_does_not_block_checkpoint_publication() {
        let (mut checkpoint, mut storage, _, config) =
            index_gc_fixture("journal-checkpoint-overlap", 32);
        checkpoint.checkpoint(&mut storage).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut metadata_config = config.clone();
        metadata_config.object_store_sim = false;
        metadata_config.object_store_endpoint = listener.local_addr().unwrap().to_string();
        metadata_config.object_store_access_key = "test-access".to_owned();
        metadata_config.object_store_secret_key = "test-secret".to_owned();
        let mut budget = Budget::new(MetadataIo::budget_bytes(&metadata_config));
        checkpoint.metadata_io = MetadataIo::new(&metadata_config, &mut budget).unwrap();
        checkpoint.schedule_commit_prune(0);
        assert!(!crate::mem::guard::forbid_alloc(|| checkpoint.advance_commit_cleanup()).unwrap());
        assert!(!checkpoint.metadata_io_ready());
        let request = checkpoint.request_checkpoint().unwrap();
        let mut published = false;
        for _ in 0..128 {
            if matches!(
                checkpoint.checkpoint_step(&mut storage).unwrap(),
                CheckpointStep::Published { .. }
            ) {
                published = true;
                break;
            }
        }
        assert!(
            published,
            "cache-saving publication must not wait for obsolete journal I/O"
        );
        assert!(checkpoint.checkpoint_request_satisfied(request));
        assert!(checkpoint.commit_cleanup_pending());
        assert!(
            !checkpoint.metadata_io_ready(),
            "the test server never answered the journal request"
        );
        drop(checkpoint);
        drop(storage);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }

    #[test]
    fn maintenance_metadata_journal_queues_do_not_alias_physical_cleanup_or_old_targets() {
        let (mut checkpoint, storage, _, config) = index_gc_fixture("journal-private-queues", 32);
        checkpoint.delete_objects_per_beat = 1;
        let mut batches = Vec::new();
        for lsn in 1..=8 {
            checkpoint
                .publish_commit_batch(lsn, b"opaque log bytes")
                .unwrap();
            batches.push(checkpoint.commit_head.unwrap());
        }
        checkpoint
            .client
            .put("sst/orphan", b"obsolete bytes", Precondition::None)
            .unwrap();
        checkpoint.schedule_commit_prune(2);
        checkpoint.schedule_commit_prune(5);
        checkpoint.schedule_commit_prune(7);
        assert_eq!(checkpoint.commit_prune_through, Some(2));
        assert_eq!(checkpoint.commit_prune_next, Some(7));
        for _ in 0..32 {
            if matches!(checkpoint.commit_prune, CommitPruneProgress::Delete { .. }) {
                break;
            }
            assert!(
                !crate::mem::guard::forbid_alloc(|| checkpoint.advance_commit_cleanup()).unwrap()
            );
        }
        assert!(matches!(
            checkpoint.commit_prune,
            CommitPruneProgress::Delete { .. }
        ));
        let queued = checkpoint.commit_garbage_scratch.clone();
        assert_eq!(queued.len(), 2);
        assert!(checkpoint.collect_garbage_batch().unwrap());
        checkpoint.load_backup_names().unwrap();
        assert_eq!(checkpoint.commit_garbage_scratch, queued);
        assert!(checkpoint.garbage_scratch.is_empty());
        assert!(checkpoint.backup_roster_loaded);
        let mut complete = false;
        for _ in 0..4 {
            complete =
                crate::mem::guard::forbid_alloc(|| checkpoint.advance_commit_cleanup()).unwrap();
            if complete {
                break;
            }
        }
        assert!(complete);
        assert_eq!(checkpoint.commit_prune_through, Some(7));
        checkpoint.schedule_commit_prune(4);
        assert_eq!(checkpoint.commit_prune_through, Some(4));
        assert_eq!(checkpoint.commit_prune_next, None);
        for _ in 0..32 {
            if !checkpoint.commit_cleanup_pending() {
                break;
            }
            crate::mem::guard::forbid_alloc(|| checkpoint.advance_commit_cleanup()).unwrap();
        }
        assert!(!checkpoint.commit_cleanup_pending());
        let namespace = crate::object_store::sim::open_namespace(&config.object_store_bucket, 0);
        for batch in batches {
            let key = format!(
                "{}commits/{:020}-{:08x}.batch",
                config.object_store_prefix, batch.first_lsn, batch.digest
            );
            assert_eq!(
                namespace.borrow().object_bytes(&key).is_some(),
                batch.first_lsn >= 4
            );
        }
        drop(checkpoint);
        drop(storage);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }

    #[test]
    fn maintenance_metadata_pruning_stops_before_a_growing_newer_commit_tail() {
        let mut config = Config::default_dev();
        config.object_store_on = true;
        config.object_store_sim = true;
        config.block_cache_bytes = 0;
        config.disk_cache_bytes = 0;
        config.checkpoint_garbage_batch_objects = 16;
        config.checkpoint_delete_objects_per_beat = 2;
        config.object_store_bucket = format!("metadata-prune-tail-{}", std::process::id());
        config.data_dir = std::env::temp_dir()
            .join(&config.object_store_bucket)
            .to_str()
            .unwrap()
            .to_owned();
        config.remove_test_data_dir_on_drop();
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let namespace = crate::object_store::sim::open_namespace(&config.object_store_bucket, 0);
        let mut budget = Budget::new(Checkpointer::budget_bytes(&config));
        let mut checkpointer = Checkpointer::new(&config, &mut budget).unwrap();
        let mut first = Vec::new();
        for lsn in 1..=100 {
            checkpointer
                .publish_commit_batch(lsn, b"confirmed log bytes")
                .unwrap();
            if lsn <= 4 {
                first.push(checkpointer.commit_head.unwrap());
            }
        }
        for _ in 0..110 {
            if matches!(checkpointer.commit_prune, CommitPruneProgress::List { .. }) {
                break;
            }
            assert!(
                !crate::mem::guard::forbid_alloc(|| checkpointer.prune_commit_batch(4)).unwrap()
            );
        }
        assert!(matches!(
            checkpointer.commit_prune,
            CommitPruneProgress::List { .. }
        ));
        for lsn in 101..=164 {
            checkpointer
                .publish_commit_batch(lsn, b"newer confirmed log bytes")
                .unwrap();
        }
        assert!(!crate::mem::guard::forbid_alloc(|| checkpointer.prune_commit_batch(4)).unwrap());
        assert!(matches!(
            checkpointer.commit_prune,
            CommitPruneProgress::Delete {
                overflow: false,
                ..
            }
        ));
        let mut done = false;
        for lsn in 165..=168 {
            checkpointer
                .publish_commit_batch(lsn, b"growing log tail")
                .unwrap();
            done = crate::mem::guard::forbid_alloc(|| checkpointer.prune_commit_batch(4)).unwrap();
            if done {
                break;
            }
        }
        assert!(done, "new commits must not extend the old cleanup decision");
        for batch in first {
            for suffix in ["batch", "head"] {
                let key = format!(
                    "{}commits/{:020}-{:08x}.{suffix}",
                    config.object_store_prefix, batch.first_lsn, batch.digest
                );
                assert_eq!(
                    namespace.borrow().object_bytes(&key).is_some(),
                    batch.first_lsn == 4
                );
            }
        }
        let latest = checkpointer.commit_head.unwrap();
        assert!(
            namespace
                .borrow()
                .object_bytes(&format!(
                    "{}commits/{:020}-{:08x}.batch",
                    config.object_store_prefix, latest.first_lsn, latest.digest
                ))
                .is_some()
        );
        drop(checkpointer);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }

    #[test]
    fn maintenance_metadata_backup_capacity_and_changes_restart_retention_safely() {
        let mut config = Config::default_dev();
        config.object_store_on = true;
        config.object_store_sim = true;
        config.block_cache_bytes = 0;
        config.disk_cache_bytes = 0;
        config.max_backups = 2;
        config.object_store_bucket = format!("metadata-backup-limit-{}", std::process::id());
        config.data_dir = std::env::temp_dir()
            .join(&config.object_store_bucket)
            .to_str()
            .unwrap()
            .to_owned();
        config.remove_test_data_dir_on_drop();
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let mut budget = Budget::new(Checkpointer::budget_bytes(&config));
        let mut checkpointer = Checkpointer::new(&config, &mut budget).unwrap();
        for (name, floor) in [("a", 3), ("a-", 4), ("old", 1)] {
            let manifest = format!(
                "{MANIFEST_HEADER}\nlsn {floor}\nnext_rowid 2\nlatest_transaction_id 1\nnext_lo_oid -\ncatalog_sequence 0\nrole_sequence 16384\nwriter 00000000000000000000000000000001\ntypes_end\nend\n"
            );
            checkpointer
                .client
                .put(
                    &format!("backups/{name}/manifest"),
                    manifest.as_bytes(),
                    Precondition::IfNoneMatchAny,
                )
                .unwrap();
        }
        let error =
            crate::mem::guard::forbid_alloc(|| checkpointer.prune_commit_batch(8)).unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(error.message.as_str().contains("max_backups (2)"));
        assert!(!checkpointer.prune_backup_roster_loaded);
        assert!(checkpointer.prune_backup_scratch.is_empty());
        assert!(checkpointer.commit_garbage_scratch.is_empty());
        assert!(checkpointer.delete_backup("old").unwrap());
        assert!(!checkpointer.prune_commit_batch(8).unwrap());
        assert_eq!(
            checkpointer
                .prune_backup_scratch
                .iter()
                .map(StackStr::as_str)
                .collect::<Vec<_>>(),
            ["a", "a-"]
        );
        assert!(matches!(
            checkpointer.commit_prune,
            CommitPruneProgress::BackupFloors {
                index: 0,
                oldest: None
            }
        ));
        assert!(!checkpointer.prune_commit_batch(8).unwrap());
        assert!(matches!(
            checkpointer.commit_prune,
            CommitPruneProgress::BackupFloors {
                index: 1,
                oldest: Some(3)
            }
        ));
        assert!(checkpointer.delete_backup("a").unwrap());
        assert!(matches!(
            checkpointer.commit_prune,
            CommitPruneProgress::Unloaded
        ));
        assert!(!checkpointer.prune_backup_roster_loaded);
        assert!(!checkpointer.prune_commit_batch(8).unwrap());
        assert!(!checkpointer.prune_commit_batch(8).unwrap());
        assert!(matches!(
            checkpointer.commit_prune,
            CommitPruneProgress::FindBoundary { floor: 4, .. }
        ));
        drop(checkpointer);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }

    #[test]
    fn maintenance_metadata_commit_names_reject_truncation_and_noncanonical_identifiers() {
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                commit_object_identity("commits/00000000000000000004-abcdef01.batch"),
                Some(CommitBatchId {
                    first_lsn: 4,
                    digest: 0xabcdef01
                })
            );
            for key in [
                "commits/4-abcdef01.batch",
                "commits/00000000000000000004-ABCDEF01.head",
                "commits/000000000000000000004-abcdef01.batch",
                "commits/00000000000000000004-abcdef01.batch.extra",
                "commits/18446744073709551616-abcdef01.head",
                "commits/é0000000000000000004-abcdef01.head",
                "other/00000000000000000004-abcdef01.batch",
            ] {
                assert!(commit_object_identity(key).is_none(), "{key}");
            }
        });
    }

    #[test]
    fn cleanup_download_overlap_pruning_walk_yields_retries_and_preserves_late_appends() {
        let mut config = Config::default_dev();
        config.object_store_on = true;
        config.object_store_sim = true;
        config.object_store_get_slots = 1;
        config.block_cache_bytes = 0;
        config.disk_cache_bytes = 0;
        config.checkpoint_garbage_batch_objects = 3;
        config.checkpoint_delete_objects_per_beat = 2;
        config.object_store_bucket = format!("paced-prune-{}", std::process::id());
        config.data_dir = std::env::temp_dir()
            .join(&config.object_store_bucket)
            .to_str()
            .unwrap()
            .to_owned();
        config.remove_test_data_dir_on_drop();
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let namespace = crate::object_store::sim::open_namespace(&config.object_store_bucket, 0);
        let mut budget = Budget::new(Checkpointer::budget_bytes(&config));
        let mut checkpointer = Checkpointer::new(&config, &mut budget).unwrap();
        let mut batches = Vec::new();
        for first in 1..=9 {
            let body = stack_format!(32, "opaque batch {first}");
            checkpointer
                .publish_commit_batch(first, body.as_str().as_bytes())
                .unwrap();
            batches.push(checkpointer.commit_head.unwrap());
        }
        let retained = batches[3];
        let orphan_digest = !retained.digest;
        let orphan_batch = format!(
            "commits/{:020}-{orphan_digest:08x}.batch",
            retained.first_lsn
        );
        let orphan_head = format!(
            "commits/{:020}-{orphan_digest:08x}.head",
            retained.first_lsn
        );
        for key in [&orphan_batch, &orphan_head] {
            checkpointer
                .client
                .put(key, b"unpublished orphan", Precondition::IfNoneMatchAny)
                .unwrap();
        }
        assert!(!checkpointer.prune_commit_batch(4).unwrap());
        for _ in 0..2 {
            let before = namespace.borrow().op_count;
            crate::mem::guard::forbid_alloc(|| {
                assert!(!checkpointer.prune_commit_batch(4).unwrap())
            });
            assert_eq!(namespace.borrow().op_count, before + 1);
        }
        let cursor = match checkpointer.commit_prune {
            CommitPruneProgress::FindBoundary { cursor, .. } => cursor,
            _ => panic!("long chain must remain in its bounded walk"),
        };
        namespace.borrow_mut().faults.fail_from_op = Some(0);
        let failure =
            crate::mem::guard::forbid_alloc(|| checkpointer.prune_commit_batch(4)).unwrap_err();
        assert_eq!(failure.sqlstate, sqlstate::IO_ERROR);
        assert!(matches!(checkpointer.commit_prune,
            CommitPruneProgress::FindBoundary { cursor: current, .. } if current == cursor));
        namespace.borrow_mut().faults.fail_from_op = None;
        let mut turns = 0;
        while matches!(
            checkpointer.commit_prune,
            CommitPruneProgress::FindBoundary { .. }
        ) {
            assert!(turns < 16);
            let before = namespace.borrow().op_count;
            crate::mem::guard::forbid_alloc(|| {
                assert!(!checkpointer.prune_commit_batch(4).unwrap())
            });
            assert_eq!(namespace.borrow().op_count, before + 1);
            turns += 1;
        }
        assert!(matches!(checkpointer.commit_prune,
            CommitPruneProgress::List { boundary: Some(boundary), .. } if boundary == retained));
        assert!(!checkpointer.prune_commit_batch(4).unwrap());
        // A later publication extends the captured chain instead of replacing it.
        checkpointer
            .publish_commit_batch(10, b"later committed batch")
            .unwrap();
        batches.push(checkpointer.commit_head.unwrap());
        let mut complete = false;
        for _ in 0..32 {
            let deleting = matches!(
                checkpointer.commit_prune,
                CommitPruneProgress::Delete { .. }
            );
            let before = namespace.borrow().op_count;
            complete = checkpointer.prune_commit_batch(4).unwrap();
            assert!(namespace.borrow().op_count - before <= if deleting { 2 } else { 1 });
            if complete {
                break;
            }
        }
        assert!(complete);
        let namespace = namespace.borrow();
        for batch in batches {
            for suffix in ["batch", "head"] {
                let key = format!(
                    "{}commits/{:020}-{:08x}.{suffix}",
                    config.object_store_prefix, batch.first_lsn, batch.digest
                );
                assert_eq!(namespace.object_bytes(&key).is_some(), batch.first_lsn >= 4);
            }
        }
        for key in [&orphan_batch, &orphan_head] {
            assert!(
                namespace
                    .object_bytes(&format!("{}{key}", config.object_store_prefix))
                    .is_none()
            );
        }
        drop(namespace);
        drop(checkpointer);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }

    #[test]
    fn cleanup_download_overlap_checkpoint_reserves_metadata_and_block_clients() {
        let mut config = Config::default_dev();
        config.object_store_on = true;
        config.object_store_endpoint = "127.0.0.1:9".to_owned();
        config.object_store_bucket = "checkpoint-request-budget".to_owned();
        config.object_store_access_key = "test-access".to_owned();
        config.object_store_secret_key = "test-secret".to_owned();
        config.object_store_head_bytes = 4096;
        config.object_store_response_bytes = crate::store::BLOCK_SIZE;
        config.block_cache_bytes = 0;
        config.disk_cache_bytes = 0;
        config.data_dir = std::env::temp_dir()
            .join(format!(
                "pos3ql-checkpoint-request-budget-{}",
                std::process::id()
            ))
            .to_str()
            .unwrap()
            .to_owned();
        config.remove_test_data_dir_on_drop();
        for slots in [1, 4, 7] {
            config.object_store_get_slots = slots;
            let mut budget = Budget::new(Checkpointer::budget_bytes(&config));
            let checkpointer = Checkpointer::new(&config, &mut budget).unwrap();
            assert_eq!(budget.remaining(), 0);
            drop(checkpointer);
        }
    }

    #[test]
    fn retained_sst_temporary_spill_constructor_charges_its_startup_budget() {
        let mut config = Config::default_dev();
        config.temporary_spill_bytes = 8 * crate::store::BLOCK_SIZE;
        config.data_dir = std::env::temp_dir()
            .join(format!("pos3ql-retained-sst-budget-{}", std::process::id()))
            .to_str()
            .unwrap()
            .to_owned();
        let mut budget = Budget::new(TemporarySpiller::budget_bytes(&config));
        let spiller = TemporarySpiller::new(&config, &mut budget).unwrap();
        assert_eq!(budget.remaining(), 0);
        drop(spiller);
        std::fs::remove_dir_all(&config.data_dir).unwrap();
    }

    #[test]
    fn schema_name_manifest_catalog_duplicates_reject_before_replacement() {
        let result = std::thread::Builder::new()
            .stack_size(crate::sql::exec::QUERY_STACK_BYTES)
            .spawn(|| {
                let mut config = Config::default_dev();
                config.object_store_sim = true;
                config.object_store_bucket = format!("database-manifest-{}", std::process::id());
                config.data_dir = std::env::temp_dir()
                    .join(format!("pos3ql-database-manifest-{}", std::process::id()))
                    .to_str()
                    .unwrap()
                    .to_owned();
                config.block_cache_bytes = 0;
                config.disk_cache_bytes = 0;
                config.memtable_bytes = 1 << 16;
                config.max_tables = 4;
                config.table_rows = 128;
                config.txn_rows = 128;
                config.value_index_rows = 512;
                config.max_value_indexes = 8;
                config.max_prepared_transactions = 2;
                let bytes = config.memtable_bytes
                    + Storage::extra_budget_bytes(&config)
                    + Checkpointer::budget_bytes(&config)
                    + (1 << 20);
                let saved = {
                    let mut budget = Budget::new(bytes);
                    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
                    let mut checkpointer = Checkpointer::new(&config, &mut budget).unwrap();
                    let mut definition = TableDef::empty();
                    definition.schema = SqlName::parse("public").unwrap();
                    definition.name = SqlName::parse("schema_manifest_table").unwrap();
                    storage.create_table(definition).unwrap();
                    checkpointer.prepare_manifest(&storage, 0).unwrap();
                    String::from_utf8(checkpointer.manifest_output.readable().to_vec()).unwrap()
                };
                for remove_namespaces in [false, true] {
                    let mut budget = Budget::new(bytes);
                    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
                    let mut checkpointer = Checkpointer::new(&config, &mut budget).unwrap();
                    let schema_acl_tag = format!("acl {} ", crate::storage::AccessClass::Schema as u8);
                    let manifest = saved.lines()
                        .filter(|line| !remove_namespaces || (!line.starts_with("nsp ") && !line.starts_with(&schema_acl_tag)))
                        .map(|line| format!("{line}\n")).collect::<String>();
                    let restored = checkpointer.load_manifest_text(&mut storage, &manifest);
                    if remove_namespaces {
                        match restored {
                            Err(CheckpointSetupError::ObjectStore(message)) =>
                                assert!(message.contains("manifest schema references rejected")),
                            other => panic!("expected missing namespace rejection: {other:?}"),
                        }
                    } else {
                        restored.unwrap();
                        assert!(storage.find_table("public", "schema_manifest_table").is_some());
                    }
                }
                for (sequence, records) in [
                    (10, "nsp 7 0 6669727374\nnsp 7 0 7365636f6e64\n"),
                    (10, "nsp 7 0 6669727374\nnsp 8 0 6669727374\n"),
                    (10, "nsp 7 0 6669727374\nnsp 11 0 7365636f6e64\n"),
                    (1_920_000, "nsp 7 0 6669727374\nnsp 1920000 0 7365636f6e64\n"),
                    (10, "nsp 7 0 6669727374\nnsp 8 2 7365636f6e64\n"),
                    (10, "nsp 7 0 6669727374\nnsp 8 0 7365636f6e64 trailing\n"),
                    (10, "nsp 7 0 6669727374\nnsp 8 0 \n"),
                    (10, "nsp 7 0 6669727374\nnsp 8 0 70675f636174616c6f67\n"),
                ] {
                    let mut budget = Budget::new(bytes);
                    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
                    let mut checkpointer = Checkpointer::new(&config, &mut budget).unwrap();
                    let manifest = format!("{MANIFEST_HEADER}\nlsn 0\nnext_rowid 2\nlatest_transaction_id 1\nnext_lo_oid -\ncatalog_sequence {sequence}\nrole_sequence 16384\nwriter 00000000000000000000000000000001\n{records}types_end\nend\n");
                    assert!(checkpointer.load_manifest_text(&mut storage, &manifest).is_err(), "{records}");
                    let slot = storage.find_schema("first").unwrap();
                    assert_eq!(storage.schema_def(slot).identity().unwrap().created_at(), 7);
                    assert_eq!(storage.find_schema("second"), None);
                    assert_eq!(storage.find_schema("public"), None);
                }
                for (first_oid, second_oid, first_name, second_name) in [
                    (5, 5, "postgres", "replacement"),
                    (5, 100_001, "postgres", "postgres"),
                    (100_001, 100_001, "first", "replacement"),
                    (100_001, 100_002, "first", "first"),
                ] {
                    let mut budget = Budget::new(bytes);
                    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
                    let mut checkpointer = Checkpointer::new(&config, &mut budget).unwrap();
                    let mut manifest = String::from("pos3ql-manifest-v34\nlsn 0\nnext_rowid 2\nlatest_transaction_id 1\nnext_lo_oid -\ncatalog_sequence 0\nrole_sequence 16384\nwriter 00000000000000000000000000000001\ntypes_end\n");
                    for (oid, name) in [(first_oid, first_name), (second_oid, second_name)] {
                        let encoded = name
                            .as_bytes()
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect::<String>();
                        manifest.push_str(&format!(
                            "db {oid} {encoded} 706f737467726573 1 6 99 0 -1 43 43 - -\n"
                        ));
                    }
                    manifest.push_str("end\n");
                    assert!(matches!(
                        checkpointer.load_manifest_text(&mut storage, &manifest),
                        Err(CheckpointSetupError::Corrupt("duplicate database record"))
                    ));
                    assert!(storage.database_named(first_name, 0).is_some());
                    if first_name != second_name {
                        assert!(storage.database_named(second_name, 0).is_none());
                    }
                }
                {
                    let mut budget = Budget::new(bytes);
                    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
                    let mut checkpointer = Checkpointer::new(&config, &mut budget).unwrap();
                    assert!(matches!(checkpointer.load_manifest_text(&mut storage,
                        "pos3ql-manifest-v34\nlsn 0\nnext_rowid 2\nlatest_transaction_id 1\nnext_lo_oid -\ncatalog_sequence 0\nrole_sequence 16384\nwriter 00000000000000000000000000000001\nsset 73616d706c65 6669727374\nsset 73616d706c65 7365636f6e64\ntypes_end\nend\n"),
                        Err(CheckpointSetupError::Corrupt("duplicate system setting record"))));
                    assert_eq!(storage.system_settings().find(|(_, image)| image.live).unwrap().1
                        .value.as_str(), "first");
                }
                for (transaction_id, gid) in [(10, "7365636f6e64"), (11, "6669727374")] {
                    let mut budget = Budget::new(bytes);
                    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
                    let mut checkpointer = Checkpointer::new(&config, &mut budget).unwrap();
                    let manifest = format!("pos3ql-manifest-v34\nlsn 0\nnext_rowid 2\nlatest_transaction_id 1\nnext_lo_oid -\ncatalog_sequence 0\nrole_sequence 16384\nwriter 00000000000000000000000000000001\nptx 10 1 2 0 5 10 6669727374\nptx {transaction_id} 1 2 0 5 10 {gid}\ntypes_end\nend\n");
                    match checkpointer.load_manifest_text(&mut storage, &manifest) {
                        Err(CheckpointSetupError::ObjectStore(message)) => {
                            assert!(message.contains("duplicate prepared transaction catalog identity"));
                        }
                        other => panic!("expected duplicate prepared transaction rejection: {other:?}"),
                    }
                    assert_eq!(storage.with_prepared_transaction_catalog(|entries| entries.len()), 1);
                    assert_eq!(storage.with_prepared_transaction_catalog(|entries| entries[0].gid)
                        .as_str(), "first");
                }
                std::fs::remove_dir_all(&config.data_dir).unwrap();
                crate::object_store::sim::drop_namespace(&config.object_store_bucket);
            })
            .unwrap()
            .join();
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }

    #[test]
    fn checkpoint_maintenance_capacities_are_exactly_budgeted() {
        let base = Config::default_dev();
        let base_bytes = Checkpointer::budget_bytes(&base);

        let mut prepared = base.clone();
        prepared.max_prepared_transactions += 1;
        assert_eq!(
            Checkpointer::budget_bytes(&prepared) - base_bytes,
            core::mem::size_of::<crate::storage::PreparedTransactionCatalogEntry>()
        );

        let mut commit = base.clone();
        commit.checkpoint_commit_batches += 1;
        assert_eq!(
            Checkpointer::budget_bytes(&commit) - base_bytes,
            core::mem::size_of::<StackStr<64>>()
        );

        let mut backups = base.clone();
        backups.max_backups += 1;
        assert_eq!(
            Checkpointer::budget_bytes(&backups) - base_bytes,
            2 * core::mem::size_of::<StackStr<BACKUP_NAME_BYTES>>()
        );

        let mut live_blocks = base.clone();
        live_blocks.checkpoint_live_blocks += 1;
        assert_eq!(
            Checkpointer::budget_bytes(&live_blocks) - base_bytes,
            core::mem::size_of::<(BlockId, Option<BlockType>)>()
                + core::mem::size_of::<(BlockId, bool)>()
                + FixedMap::<BlockId, ()>::budget_bytes(live_blocks.checkpoint_live_blocks)
                - FixedMap::<BlockId, ()>::budget_bytes(base.checkpoint_live_blocks)
        );

        let mut value_indexes = base.clone();
        value_indexes.max_value_indexes += 1;
        assert_eq!(
            Checkpointer::budget_bytes(&value_indexes) - base_bytes,
            3 * core::mem::size_of::<BlockId>()
        );

        let mut table_rows = base.clone();
        table_rows.table_rows += 1;
        assert_eq!(
            Checkpointer::budget_bytes(&table_rows) - base_bytes,
            core::mem::size_of::<u64>() + CheckpointValueCursor::budget_bytes(0, 1)
        );

        let mut garbage = base.clone();
        garbage.checkpoint_garbage_batch_objects += 1;
        assert_eq!(
            Checkpointer::budget_bytes(&garbage) - base_bytes,
            core::mem::size_of::<StackStr<80>>() + 2 * core::mem::size_of::<StackStr<64>>()
        );

        let mut delete_pacing = base.clone();
        delete_pacing.checkpoint_delete_objects_per_beat += 1;
        assert_eq!(Checkpointer::budget_bytes(&delete_pacing), base_bytes);

        let mut writes = base.clone();
        writes.checkpoint_write_entries += 1;
        assert_eq!(
            Checkpointer::budget_bytes(&writes) - base_bytes,
            core::mem::size_of::<CheckpointVersion>()
        );

        let mut manifest = base.clone();
        manifest.checkpoint_manifest_bytes += 64;
        assert_eq!(
            Checkpointer::budget_bytes(&manifest) - base_bytes,
            3 * 64,
            "metadata read scratch, retained output and immutable request body"
        );

        let mut merge = base;
        merge.checkpoint_merge_entries += 1;
        assert_eq!(
            Checkpointer::budget_bytes(&merge) - base_bytes,
            core::mem::size_of::<(SstKey, u8)>()
        );
    }

    #[test]
    fn posting_sort_groups_duplicate_row_versions_after_their_token() {
        fn entry(key: [u8; 9], rowid: u64, lsn: u64) -> [u8; VALUE_SORT_ENTRY_HEADER + 9] {
            let mut entry = [0; VALUE_SORT_ENTRY_HEADER + 9];
            entry[8..16].copy_from_slice(&rowid.to_le_bytes());
            entry[16..24].copy_from_slice(&lsn.to_le_bytes());
            entry[24..28].copy_from_slice(&9u32.to_le_bytes());
            entry[VALUE_SORT_ENTRY_HEADER..].copy_from_slice(&key);
            entry
        }

        let first = entry([1; 9], 7, 9);
        let duplicate = entry([1; 9], 7, 9);
        let other_row = entry([1; 9], 8, 9);
        let other_token = entry([2; 9], 1, 1);
        assert_eq!(
            compare_posting_sort_entries(&first, &duplicate).unwrap(),
            Ordering::Equal
        );
        assert!(
            compare_posting_sort_entries(&first, &other_row)
                .unwrap()
                .is_lt()
        );
        assert!(
            compare_posting_sort_entries(&other_row, &other_token)
                .unwrap()
                .is_lt()
        );
    }

    #[test]
    fn current_manifest_metadata_rejects_missing_duplicate_and_old_fields() {
        use core::fmt::Write as _;
        crate::mem::guard::forbid_alloc(|| {
            let current = stack_format!(
                512,
                "{MANIFEST_HEADER}\nlsn 7\nnext_rowid 2\nlatest_transaction_id 1\nnext_lo_oid -\ncatalog_sequence 0\nrole_sequence 16384\nwriter {:032x}\ntypes_end\nend\n",
                1u128
            );
            let parsed = parse_manifest_metadata(current.as_str()).unwrap();
            assert_eq!(parsed.lsn, 7);
            assert_eq!(parsed.next_rowid, 2);
            assert_eq!(parsed.latest_transaction_id, 1);
            assert_eq!(parsed.next_large_object_oid, None);
            assert_eq!(parsed.writer, WriterToken(1));
            assert_eq!(
                &current.as_str()[parsed.writer_range.0..parsed.writer_range.1],
                "writer 00000000000000000000000000000001"
            );
            let (_, body) = current.as_str().split_once('\n').unwrap();
            for header in [
                "",
                "pos3ql-manifest-v12",
                "pos3ql-manifest-v13",
                "pos3ql-manifest-v14",
                "pos3ql-manifest-v15",
                "pos3ql-manifest-v18",
                "pos3ql-manifest-v19",
                "pos3ql-manifest-v20",
                "pos3ql-manifest-v22",
                "pos3ql-manifest-v23",
                "pos3ql-manifest-v24",
                "pos3ql-manifest-v25",
                "pos3ql-manifest-v26",
                "pos3ql-manifest-v27",
                "pos3ql-manifest-v28",
                "pos3ql-manifest-v29",
                "pos3ql-manifest-v30",
                "pos3ql-manifest-v31",
                "pos3ql-manifest-v32",
                "pos3ql-manifest-v33",
                "pos3ql-manifest-v34 trailing",
            ] {
                let rejected = stack_format!(512, "{header}\n{body}");
                assert!(matches!(
                    parse_manifest_metadata(rejected.as_str()),
                    Err("unsupported manifest format")
                ));
                assert_eq!(
                    manifest_commit_retention_floor(rejected.as_str().as_bytes()),
                    Err("unsupported manifest format")
                );
            }
            assert!(matches!(
                parse_manifest_metadata(""),
                Err("unsupported manifest format")
            ));
            for (field, missing, duplicate) in [
                ("lsn ", "manifest LSN missing", "duplicate manifest LSN"),
                (
                    "next_rowid ",
                    "manifest lacks row identity allocator",
                    "duplicate row identity allocator",
                ),
                (
                    "latest_transaction_id ",
                    "manifest lacks transaction identity allocator",
                    "duplicate transaction identity allocator",
                ),
                (
                    "role_sequence ",
                    "manifest lacks role identity allocator",
                    "duplicate role identity allocator",
                ),
                (
                    "next_lo_oid ",
                    "manifest lacks large-object allocator",
                    "duplicate large-object allocator",
                ),
                (
                    "writer ",
                    "manifest writer identity missing",
                    "manifest has an invalid writer identity",
                ),
            ] {
                let mut rejected = StackStr::<512>::new();
                for line in current.as_str().split_inclusive('\n') {
                    if !line.starts_with(field) {
                        rejected.write_str(line).unwrap();
                    }
                }
                assert!(
                    matches!(parse_manifest_metadata(rejected.as_str()), Err(message) if message == missing)
                );
                assert_eq!(
                    manifest_commit_retention_floor(rejected.as_str().as_bytes()),
                    Err(missing)
                );
                let line = current
                    .as_str()
                    .split_inclusive('\n')
                    .find(|line| line.starts_with(field))
                    .unwrap();
                let prefix = current.as_str().strip_suffix("end\n").unwrap();
                let repeated = stack_format!(512, "{prefix}{line}end\n");
                assert!(
                    matches!(parse_manifest_metadata(repeated.as_str()), Err(message) if message == duplicate)
                );
            }
            let prefix = current.as_str().strip_suffix("end\n").unwrap();
            for rejected in [
                stack_format!(512, "{prefix}"),
                stack_format!(512, "{current}lsn 8\n"),
                stack_format!(512, "{prefix}end extra\n"),
            ] {
                assert!(parse_manifest_metadata(rejected.as_str()).is_err());
                assert!(manifest_commit_retention_floor(rejected.as_str().as_bytes()).is_err());
            }
        });
    }

    #[test]
    fn backup_commit_floor_includes_prepared_transactions_and_logical_slots() {
        let manifest = b"pos3ql-manifest-v34\nlsn 100\nnext_rowid 2\nlatest_transaction_id 1\nnext_lo_oid -\ncatalog_sequence 0\nrole_sequence 16384\nwriter 00000000000000000000000000000001\nptx 7 41 50 0 5 10 78\nrslot 736c6f74 60 70 0\ntypes_end\nend\n";
        assert_eq!(manifest_lsn(manifest).unwrap(), 100);
        assert_eq!(manifest_commit_retention_floor(manifest).unwrap(), 40);

        let slot_only = b"pos3ql-manifest-v34\nlsn 100\nnext_rowid 2\nlatest_transaction_id 1\nnext_lo_oid -\ncatalog_sequence 0\nrole_sequence 16384\nwriter 00000000000000000000000000000001\nrslot 736c6f74 60 70 0\ntypes_end\nend\n";
        assert_eq!(manifest_commit_retention_floor(slot_only).unwrap(), 60);
    }

    #[test]
    fn durable_format_documentation_matches_the_executable_matrix() {
        let document = include_str!("../docs/durable-format.md");
        let manifest_rows = document
            .lines()
            .filter(|line| line.starts_with("| Checkpoint manifest |"))
            .collect::<Vec<_>>();
        let row_sst_rows = document
            .lines()
            .filter(|line| line.starts_with("| Published row SST |"))
            .collect::<Vec<_>>();

        assert_eq!(manifest_rows.len(), 1);
        assert!(
            manifest_rows[0].starts_with(&format!(
                "| Checkpoint manifest | `{MANIFEST_HEADER}` | yes | yes |"
            )),
            "durable format matrix must name the current manifest reader and writer"
        );

        assert_eq!(row_sst_rows.len(), RowSstFormat::SUPPORTED.len());
        for format in RowSstFormat::SUPPORTED {
            let writable = if format == RowSstFormat::CURRENT {
                "yes"
            } else {
                "no"
            };
            let prefix = format!(
                "| Published row SST | `{}` | yes | {writable} |",
                format.manifest_id()
            );
            assert!(
                row_sst_rows.iter().any(|row| row.starts_with(&prefix)),
                "durable format matrix lacks {prefix}"
            );
        }
    }

    #[test]
    fn dsst_manifest_retains_supported_format_identity() {
        let id = "00".repeat(32);
        let mut direct = "v2".split(' ');
        assert_eq!(
            parse_dsst_handle(&id, &id, &id, &mut direct)
                .unwrap()
                .unwrap()
                .format,
            RowSstFormat::DirectV2
        );
        let mut packed = "v3".split(' ');
        assert_eq!(
            parse_dsst_handle(&id, &id, &id, &mut packed)
                .unwrap()
                .unwrap()
                .format,
            RowSstFormat::PackedPaxV3
        );
        let mut current = "v4".split(' ');
        assert_eq!(
            parse_dsst_handle(&id, &id, &id, &mut current)
                .unwrap()
                .unwrap()
                .format,
            RowSstFormat::PackedV4
        );
        let mut obsolete = "v1".split(' ');
        assert!(parse_dsst_handle(&id, &id, &id, &mut obsolete).is_err());
        let mut incomplete = core::iter::empty();
        assert!(parse_dsst_handle(&id, "-", &id, &mut incomplete).is_err());
        let mut empty = core::iter::empty();
        assert!(
            parse_dsst_handle("-", "-", "-", &mut empty)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn manifest_round_trip_preserves_reference_names() {
        let mut dependencies = StoredQueryDependencies::EMPTY;
        dependencies
            .serialized_push(SerializedStoredQueryDependency {
                class: DependencyClass::Table,
                identity: StoredDependencyIdentity::Name,
                schema: SqlName::parse("moved").unwrap(),
                name: SqlName::parse("current_name").unwrap(),
                referenced_schema: SqlName::parse("").unwrap(),
                referenced_name: SqlName::parse("original_name").unwrap(),
                referenced_columns: crate::storage::ColumnSet::from_low_word(0b101),
            })
            .unwrap();
        dependencies
            .serialized_push(SerializedStoredQueryDependency {
                class: DependencyClass::Routine,
                identity: StoredDependencyIdentity::RoutineOid(
                    crate::storage::ROUTINE_OID_BASE + 7,
                ),
                schema: SqlName::parse("public").unwrap(),
                name: SqlName::parse("expanded").unwrap(),
                referenced_schema: SqlName::parse("").unwrap(),
                referenced_name: SqlName::parse("original_function").unwrap(),
                referenced_columns: crate::storage::ColumnSet::EMPTY,
            })
            .unwrap();
        let encoded = format!("{}", ManifestDependencies(dependencies.view()));
        let mut words = encoded.split(' ');
        assert_eq!(
            parse_stored_query_dependencies(&mut words).unwrap(),
            dependencies
        );
        for (class, key) in [
            (DependencyClass::Domain, "d:1:41"),
            (DependencyClass::Enum, "e:2:42"),
            (DependencyClass::Composite, "c:3:43"),
        ] {
            let mut dependencies = StoredQueryDependencies::EMPTY;
            dependencies
                .serialized_push(SerializedStoredQueryDependency {
                    class,
                    identity: StoredDependencyIdentity::from_storage(class, key).unwrap(),
                    schema: SqlName::parse("public").unwrap(),
                    name: SqlName::parse("current_type").unwrap(),
                    referenced_schema: SqlName::parse("").unwrap(),
                    referenced_name: SqlName::parse("original_type").unwrap(),
                    referenced_columns: crate::storage::ColumnSet::EMPTY,
                })
                .unwrap();
            let encoded = format!("{}", ManifestDependencies(dependencies.view()));
            assert_eq!(
                parse_stored_query_dependencies(&mut encoded.split(' ')).unwrap(),
                dependencies
            );
        }
    }

    #[test]
    fn uploaded_wal_record_reconstruction_keeps_the_full_commit_frame() {
        let mut budget = Budget::new(1024);
        let mut scratch = FixedBuf::new(&mut budget, "replication scratch", 128).unwrap();
        let mut record = [37, 0, 0, 0, 0, 0, 0, 0, 9, 0, 0, 0];
        record[1] = crate::wal::RECORD_VERSION;
        append_uploaded_wal_record(&mut scratch, 44, &record).unwrap();
        let frame = scratch.readable();
        assert_eq!(frame.len(), crate::wal::HEADER_LEN + 4);
        assert_eq!(u64::from_le_bytes(frame[8..16].try_into().unwrap()), 44);
        assert_eq!(
            crate::wal::crc32c::crc32c(&frame[4..]),
            u32::from_le_bytes(frame[..4].try_into().unwrap())
        );
        assert!(matches!(
            crate::wal::decode_record(&frame[16..]),
            Some(crate::wal::WalOp::Commit {
                transaction_id: 9,
                assigned_transaction_identity: false,
                ..
            })
        ));
    }

    #[test]
    fn commit_descriptor_binds_its_lsn_and_digest_together() {
        let descriptor =
            b"pos3ql-commit-head-v1\nfirst 42\ndigest 00aabbcc\nprevious 7 00112233\nend\n";
        assert_eq!(
            parse_commit_descriptor(descriptor).unwrap(),
            (
                CommitBatchId {
                    first_lsn: 42,
                    digest: 0x00aa_bbcc,
                },
                Some(CommitBatchId {
                    first_lsn: 7,
                    digest: 0x0011_2233,
                }),
            )
        );
        assert!(
            parse_commit_descriptor(
                b"pos3ql-commit-head-v1\nfirst 42\ndigest 00aabbcc\nprevious 0 00112233\nend\n"
            )
            .is_err()
        );
    }

    #[test]
    fn writer_fence_and_commit_head_parse_at_their_versioned_boundaries() {
        let token = WriterToken(0x0011_2233_4455_6677_8899_aabb_ccdd_eeff);
        let active = writer_fence_record("active", token);
        assert_eq!(
            parse_writer_fence(active.as_str().as_bytes()).unwrap(),
            ("active", token)
        );
        assert!(
            parse_writer_fence(
                b"pos3ql-writer-fence-v1\nstate unknown\nwriter 00112233445566778899aabbccddeeff\nend\n"
            )
            .is_err()
        );

        let legacy =
            b"pos3ql-commit-head-v1\nwriter 0011223344556677\nfirst 42\ndigest 00aabbcc\nend\n";
        assert_eq!(
            parse_commit_head(legacy).unwrap(),
            (
                WriterToken(0x0011_2233_4455_6677),
                CommitBatchId {
                    first_lsn: 42,
                    digest: 0x00aa_bbcc,
                }
            )
        );
        let current = format_commit_head(
            token,
            CommitBatchId {
                first_lsn: 42,
                digest: 0x00aa_bbcc,
            },
        );
        assert_eq!(
            parse_commit_head(current.as_str().as_bytes()).unwrap().0,
            token
        );
    }

    #[test]
    fn promotion_validated_roots_reject_changes_before_displacing_a_writer() {
        let mut config = Config::default_dev();
        config.object_store_on = true;
        config.object_store_sim = true;
        config.object_store_bucket = format!("validated-root-promotion-{}", std::process::id());
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let mut budget = Budget::new(8 << 20);
        let mut client = ObjectStore::new(&config, &mut budget).unwrap();
        let mut manifest =
            FixedBuf::new(&mut budget, "validated promotion manifest", 1024).unwrap();
        let mut head = FixedBuf::new(&mut budget, "validated promotion head", 128).unwrap();
        let active = writer_fence_record("active", WriterToken(1));
        client
            .put(
                WRITER_FENCE_KEY,
                active.as_str().as_bytes(),
                Precondition::IfNoneMatchAny,
            )
            .unwrap();
        let original_manifest = empty_manifest_record(WriterToken(1));
        let original_head = empty_commit_head_record(WriterToken(1));
        let manifest_etag = client
            .put(
                MANIFEST_KEY,
                original_manifest.as_str().as_bytes(),
                Precondition::IfNoneMatchAny,
            )
            .unwrap();
        let head_etag = client
            .put(
                COMMIT_HEAD_KEY,
                original_head.as_str().as_bytes(),
                Precondition::IfNoneMatchAny,
            )
            .unwrap();
        let changed_manifest = empty_manifest_record(WriterToken(2));
        let changed_manifest_etag = client
            .put(
                MANIFEST_KEY,
                changed_manifest.as_str().as_bytes(),
                Precondition::IfMatch(&manifest_etag),
            )
            .unwrap();
        let error = promote_writer(
            &mut client,
            WriterToken(3),
            &mut manifest,
            &mut head,
            PromotionRoots::Validated {
                manifest: Some(manifest_etag),
                head: Some(head_etag),
            },
        )
        .err()
        .unwrap();
        assert!(
            matches!(error, CheckpointSetupError::ObjectStore(message) if message == "manifest changed after startup validation")
        );
        client.get(WRITER_FENCE_KEY, None).unwrap();
        assert_eq!(client.body_bytes(), active.as_str().as_bytes());
        client.get(MANIFEST_KEY, None).unwrap();
        assert_eq!(client.body_bytes(), changed_manifest.as_str().as_bytes());
        let changed_head = empty_commit_head_record(WriterToken(2));
        let changed_head_etag = client
            .put(
                COMMIT_HEAD_KEY,
                changed_head.as_str().as_bytes(),
                Precondition::IfMatch(&head_etag),
            )
            .unwrap();
        let error = promote_writer(
            &mut client,
            WriterToken(3),
            &mut manifest,
            &mut head,
            PromotionRoots::Validated {
                manifest: Some(changed_manifest_etag),
                head: Some(head_etag),
            },
        )
        .err()
        .unwrap();
        assert!(
            matches!(error, CheckpointSetupError::ObjectStore(message) if message == "commit-head changed after startup validation")
        );
        client.get(WRITER_FENCE_KEY, None).unwrap();
        assert_eq!(client.body_bytes(), active.as_str().as_bytes());
        client.get(COMMIT_HEAD_KEY, None).unwrap();
        assert_eq!(client.body_bytes(), changed_head.as_str().as_bytes());
        let promoted = promote_writer(
            &mut client,
            WriterToken(3),
            &mut manifest,
            &mut head,
            PromotionRoots::Validated {
                manifest: Some(changed_manifest_etag),
                head: Some(changed_head_etag),
            },
        )
        .unwrap();
        assert_eq!(
            client.get(MANIFEST_KEY, None).unwrap().etag,
            promoted.manifest
        );
        assert_eq!(
            empty_manifest_writer(client.body_bytes()).unwrap(),
            Some(WriterToken(3))
        );
        assert_eq!(
            client.get(COMMIT_HEAD_KEY, None).unwrap().etag,
            promoted.head
        );
        assert_eq!(
            empty_commit_head_writer(client.body_bytes()).unwrap(),
            Some(WriterToken(3))
        );
        client.get(WRITER_FENCE_KEY, None).unwrap();
        assert_eq!(
            parse_writer_fence(client.body_bytes()).unwrap(),
            ("active", WriterToken(3))
        );
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }

    #[test]
    fn promotion_retags_mutable_roots_even_when_etags_follow_content() {
        let old = WriterToken(1);
        let replacement = WriterToken(2);
        assert_eq!(empty_manifest_record(replacement).len(), 64);
        let mut budget = Budget::new(4096);
        let mut manifest = FixedBuf::new(&mut budget, "manifest", 1024).unwrap();
        manifest
            .append(
                b"pos3ql-manifest-v34\nlsn 7\nnext_rowid 2\nlatest_transaction_id 1\nnext_lo_oid -\ncatalog_sequence 0\nrole_sequence 16384\nwriter 00000000000000000000000000000001\ntypes_end\nend\n",
            );
        let before = manifest.readable().to_vec();
        retag_manifest_root(&mut manifest, replacement).unwrap();
        assert_ne!(manifest.readable(), before);
        let metadata =
            parse_manifest_metadata(core::str::from_utf8(manifest.readable()).unwrap()).unwrap();
        assert_eq!(metadata.writer, replacement);

        let mut rejected = FixedBuf::new(&mut budget, "rejected manifest", 256).unwrap();
        for header in [
            "pos3ql-manifest-v13",
            "pos3ql-manifest-v14",
            "pos3ql-manifest-v15",
            "pos3ql-manifest-v18",
            "pos3ql-manifest-v19",
            "pos3ql-manifest-v20",
            "pos3ql-manifest-v22",
            "pos3ql-manifest-v23",
            "pos3ql-manifest-v24",
            "pos3ql-manifest-v25",
            "pos3ql-manifest-v26",
            "pos3ql-manifest-v27",
            "pos3ql-manifest-v28",
            "pos3ql-manifest-v29",
        ] {
            rejected.clear();
            rejected.append(format!("{header}\nlsn 7\nend\n").as_bytes());
            let before = rejected.readable().to_vec();
            assert!(matches!(
                retag_manifest_root(&mut rejected, replacement),
                Err(CheckpointSetupError::Corrupt("unsupported manifest format"))
            ));
            assert_eq!(rejected.readable(), before);
        }

        let mut missing = FixedBuf::new(&mut budget, "missing manifest writer", 256).unwrap();
        for line in core::str::from_utf8(&before).unwrap().split_inclusive('\n') {
            if !line.starts_with("writer ") {
                assert!(missing.append(line.as_bytes()));
            }
        }
        let unchanged = missing.readable().to_vec();
        let error =
            crate::mem::guard::forbid_alloc(|| retag_manifest_root(&mut missing, replacement));
        assert!(matches!(
            error,
            Err(CheckpointSetupError::Corrupt(
                "manifest writer identity missing"
            ))
        ));
        assert_eq!(missing.readable(), unchanged);

        let mut head = FixedBuf::new(&mut budget, "commit head", 128).unwrap();
        head.append(
            format_commit_head(
                old,
                CommitBatchId {
                    first_lsn: 8,
                    digest: 9,
                },
            )
            .as_str()
            .as_bytes(),
        );
        let before = head.readable().to_vec();
        retag_commit_head_root(&mut head, replacement).unwrap();
        assert_ne!(head.readable(), before);
        assert_eq!(parse_commit_head(head.readable()).unwrap().0, replacement);

        head.clear();
        head.append(EMPTY_COMMIT_HEAD);
        retag_commit_head_root(&mut head, replacement).unwrap();
        assert_eq!(
            empty_commit_head_writer(head.readable()),
            Ok(Some(replacement))
        );
    }

    #[test]
    fn backup_completion_reads_v1_and_writes_timestamped_v2_shape() {
        assert_eq!(
            parse_backup_complete(
                b"pos3ql-backup-v1\nlsn 42\nmanifest_crc 00aabbcc\nhead_crc 00112233\nend\n"
            )
            .unwrap(),
            (42, None, 0x00aa_bbcc, 0x0011_2233)
        );
        assert_eq!(
            parse_backup_complete(
                b"pos3ql-backup-v2\nlsn 42\ncreated_at 820497600123456\nmanifest_crc 00aabbcc\nhead_crc 00112233\nend\n"
            )
            .unwrap(),
            (42, Some(820_497_600_123_456), 0x00aa_bbcc, 0x0011_2233)
        );
    }
}

#[cfg(test)]
#[test]
fn parallel_physical_emission_covers_maximum_chained_row_and_finish() {
    let mut config = Config::default_dev();
    config.object_store_on = true;
    config.object_store_sim = false;
    config.object_store_endpoint = "127.0.0.1:1".to_string();
    config.object_store_access_key = "access".to_string();
    config.object_store_secret_key = "secret".to_string();
    config.object_store_bucket = format!("physical-emission-bound-{}", std::process::id());
    let total = SstWriter::budget_bytes()
        + crate::store::BlockWriteBatch::budget_bytes(
            PHYSICAL_EMISSION_BYTES,
            PHYSICAL_EMISSION_OBJECTS,
        )
        + crate::object_store::upload::Uploads::budget_bytes(&config, 1, crate::store::BLOCK_SIZE);
    let mut budget = Budget::new(total);
    budget
        .draw(SstWriter::budget_bytes(), "physical emission test writer")
        .unwrap();
    let mut writer = SstWriter::new();
    let mut output = crate::store::BlockWriteBatch::new(
        &mut budget,
        PHYSICAL_EMISSION_BYTES,
        PHYSICAL_EMISSION_OBJECTS,
    )
    .unwrap();
    let mut uploads = crate::object_store::upload::Uploads::new(
        &config,
        1,
        crate::store::BLOCK_SIZE,
        &mut budget,
    )
    .unwrap();
    assert_eq!(budget.used(), total);
    let mut row = vec![0u8; crate::store::MAX_ASSEMBLED];
    let mut random = 0x6a09e667f3bcc909u64;
    for byte in &mut row {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        *byte = random as u8;
    }
    crate::mem::guard::forbid_alloc(|| {
        writer.append(&mut output, 0, b"prior group").unwrap();
        writer.append(&mut output, 1, &row).unwrap();
        assert!(!output.is_empty());
        output.discard(&mut uploads).unwrap();
        assert!(writer.finish(&mut output).unwrap().is_some());
        assert!(!output.is_empty());
        output.discard(&mut uploads).unwrap();
    });
}

#[cfg(test)]
#[test]
fn parallel_physical_credential_rotation_waits_for_requests_not_retained_bytes() {
    use crate::store::BlockWriter;
    let mut config = Config::default_dev();
    config.object_store_on = true;
    config.object_store_sim = false;
    config.object_store_endpoint = "127.0.0.1:1".to_string();
    config.object_store_access_key = "access".to_string();
    config.object_store_secret_key = "secret".to_string();
    config.object_store_bucket = format!("physical-credential-owner-{}", std::process::id());
    config.data_dir = std::env::temp_dir()
        .join(&config.object_store_bucket)
        .to_string_lossy()
        .into_owned();
    config.max_tables = 2;
    config.table_rows = 4;
    config.block_cache_bytes = 0;
    config.disk_cache_bytes = 0;
    let mut budget = Budget::new(Checkpointer::budget_bytes(&config));
    let mut checkpoint = Checkpointer::new(&config, &mut budget).unwrap();
    assert_eq!(budget.used(), Checkpointer::budget_bytes(&config));
    crate::mem::guard::forbid_alloc(|| {
        checkpoint
            .block_output
            .write_block(b"retained", crate::store::BlockType::SstData, 0)
            .unwrap();
        assert!(!checkpoint.block_output.is_empty());
        assert!(
            !checkpoint.object_io_busy(),
            "retained output must allow credential reload between attempts"
        );
        assert!(!checkpoint.drain_block_output().unwrap());
        assert!(
            checkpoint.object_io_busy(),
            "queued PUT owns its current request credentials"
        );
        checkpoint
            .block_output
            .discard(&mut checkpoint.block_uploads)
            .unwrap();
        assert!(!checkpoint.object_io_busy());
    });
    drop(checkpoint);
    std::fs::remove_dir_all(&config.data_dir).unwrap();
}
