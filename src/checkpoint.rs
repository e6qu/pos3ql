//! Immutable SST publication and cold recovery through the manifest CAS.

use crate::config::Config;
use crate::mem::arena::Arena;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::buffer::FixedBuf;
use crate::mem::fixed_vec::FixedVec;
use crate::object_store::{
    ByteRange, Client as ObjectStore, EntityTag, Error as ObjectError, Precondition,
};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::types::ColType;
use crate::sql_err;
use crate::stack_format;
use crate::storage::{
    ColumnDefault, ColumnMeta, MAX_COLUMNS, OwnedDatum, PartitionBound, PartitionBoundValue,
    PartitionDef, PartitionStrategy, RowHome, SerializedStoredQueryDependency, SqlName, Storage,
    TableDef,
};
use crate::store::{
    BlockId, BlockStore, OwnedObjectStore, SstHandle, SstKey, SstReader, SstWriter, StackPlan,
    TieredStore, ValueIndexHandle, ValueIndexWriter,
};
use crate::util::StackStr;
use crate::wal::crc32c::Crc32c;

pub(crate) const MANIFEST_KEY: &str = "manifest";
const COMMIT_HEAD_KEY: &str = "commit-head";
const COMMIT_HEAD_HEADER: &str = "pos3ql-commit-head-v1";
const MANIFEST_HEADER: &str = "pos3ql-manifest-v4";
const EXTENSION_PACKAGE_HEADER: &str = "pos3ql-extension-package-v1";
const MANIFEST_BUF_BYTES: usize = 256 * 1024;
const VERSIONED_SST_ENTRY_HEADER: usize = 20; // rowid u64 | commit_lsn u64 | len u32

/// io_error — object storage trouble surfaced to a statement.
const SQLSTATE_IO: &str = "58030";
/// serialization_failure — manifest CAS lost to another writer.
const SQLSTATE_CAS: &str = "40001";

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
#[derive(Clone, Copy)]
enum SlotInstall {
    Append(SstHandle),
    Collapse(SstHandle),
    /// Paced compaction merged the adjacent pair at list positions
    /// (`at`, `at + 1`) into one (`None` when everything in the pair was
    /// deleted): remap in-memory spill indexes.
    MergePair {
        at: usize,
        handle: Option<SstHandle>,
    },
}

#[derive(Clone, Copy)]
struct ValueInstall {
    slot: usize,
    columns: [u16; crate::storage::MAX_INDEX_COLS],
    n_columns: usize,
    handle: Option<ValueIndexHandle>,
}

/// A prior checkpoint's SST reference for one table slot.
#[derive(Clone, Copy)]
struct PrevSst {
    handle: SstHandle,
    count: u64,
    crc: u32,
}

/// One table's published SST list — a fixed, `Copy` array so the post-freeze
/// checkpoint path never touches the allocator.
#[derive(Clone, Copy)]
struct SlotList {
    ssts: [Option<PrevSst>; crate::storage::MAX_SPILL_SSTS],
    n: usize,
}

impl SlotList {
    const EMPTY: SlotList = SlotList {
        ssts: [None; crate::storage::MAX_SPILL_SSTS],
        n: 0,
    };

    fn push(&mut self, p: PrevSst) -> bool {
        if self.n == crate::storage::MAX_SPILL_SSTS {
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
struct CompletedMerge {
    slot: usize,
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
            "spill list exceeds its fixed capacity"
        ));
    }
    Ok(())
}

pub(crate) struct Checkpointer {
    pub(crate) client: ObjectStore,
    /// The block-grid path to the bucket: RAM frames over a disk slot file
    /// over content-addressed block objects — `block_cache_bytes` and
    /// `disk_cache_bytes` finally sized to something. SST reads and writes go
    /// through here; writes populate the tiers on the way out, so a cold
    /// start warms what a later read wants. Shared with the storage layer's
    /// spilled-row reader (single-threaded engine, short borrows).
    blocks: std::rc::Rc<std::cell::RefCell<TieredStore<OwnedObjectStore>>>,
    /// Scratch for SST writers and readers, reset per table.
    sst_arena: Arena,
    /// Spill-list updates computed during a checkpoint, applied to storage
    /// only after the manifest CAS lands.
    pending_installs: Vec<(usize, SlotInstall)>,
    pending_value_installs: Vec<ValueInstall>,
    /// Pre-reserved physical-version schedule: (key, source-and-kind).
    merge_scratch: Vec<(SstKey, u8)>,
    /// Rosters of the SSTs the current manifest references (GC keep-set
    /// source) and their sweep scratch.
    roster_scratch: Vec<BlockId>,
    doomed_blocks: Vec<StackStr<80>>,
    manifest_buf: FixedBuf,
    manifest_etag: Option<EntityTag>,
    manifest_lsn: u64,
    /// CAS-published root of the immutable commit-batch chain.  A batch PUT
    /// alone is intentionally not recoverable until this pointer advances.
    commit_head_etag: Option<EntityTag>,
    commit_head: Option<CommitBatchId>,
    /// Per-slot SST from the last published manifest; clean tables reuse
    /// these handles (delta checkpoints). Capacity is reserved at startup so
    /// the post-freeze checkpoint path never allocates.
    prev_ssts: Vec<SlotList>,
    /// Keys referenced by the manifest just published (GC keep-set).
    referenced: Vec<StackStr<64>>,
    /// Pre-reserved scratch built during a checkpoint, then swapped into the
    /// fields above; keeps the post-freeze path allocation-free.
    prev_scratch: Vec<SlotList>,
    ref_scratch: Vec<StackStr<64>>,
    /// Pre-reserved scratch for GC / WAL-segment sweeps.
    doomed_scratch: Vec<StackStr<64>>,
    /// Sliced-checkpoint sweep state: whether a sweep is mid-flight, the
    /// table generation each slot's slice captured, and which slots were
    /// sliced this sweep.
    sweeping: bool,
    /// A manifest is durable but its bounded garbage sweep still needs a
    /// retry. Its LSN is withheld until maintenance completes.
    published_lsn_pending_maintenance: Option<u64>,
    sliced_generation: Vec<u64>,
    sliced_this_sweep: Vec<bool>,
    /// The slice writer (reset per table) and the merge writer, which holds
    /// a half-written SST across beats — the reason the writer owns its
    /// state instead of borrowing an arena.
    slice_writer: SstWriter,
    merge_writer: SstWriter,
    value_writer: ValueIndexWriter,
    merge_job: Option<MergeJob>,
    merge_done: Option<CompletedMerge>,
    /// Fairness toggle: merge beats and sweep beats alternate when both
    /// want the engine, so neither starves the other.
    merge_turn: bool,
    /// Pairs whose scans overflowed the id scratch (their stored counts
    /// under-reported); remembered per slot so the scheduler stops
    /// proposing a merge that cannot be scheduled.
    merge_overflow: Vec<Option<(BlockId, BlockId)>>,
    /// This database's writer identity, stamped into every manifest it
    /// publishes (`writer <hex>`). Deterministic from the node's identity
    /// (bucket, key prefix, data directory), so every incarnation of the
    /// same node shares it and two nodes pointed at one bucket do not. Its
    /// job is disambiguating a failed compare-and-swap: a manifest carrying
    /// our id was our own PUT whose response was lost — adopt its etag and
    /// republish; any other id is a genuine second writer, which stays a
    /// loud error.
    writer_id: u64,
}

/// One beat's outcome: nothing to publish, a slice written, or the manifest
/// published at `lsn`.
pub(crate) enum CheckpointStep {
    Idle,
    Working,
    Published { lsn: u64 },
}

/// Upper bounds reserved at startup so checkpoint-time bookkeeping never
/// touches the allocator. Exhausting one is a named checkpoint error; success
/// must mean every requested maintenance operation completed.
const MAX_CKPT_TABLES: usize = 1024;
const MAX_SWEEP_KEYS: usize = 4096;
/// Block identities the GC keep-set can hold across every live SST.
const MAX_KEEP_BLOCKS: usize = 64 * 1024;
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

/// Merge id-scratch capacity, in (rowid, source) entries. Sized generously
/// past a full table plus its tombstone backlog; a pair whose combined count
/// exceeds it skips its merge that cycle. A filled list has one mandatory
/// full-rewrite transition.
const MERGE_SCRATCH_ENTRIES: usize = 512 * 1024;

/// How far one merge beat may go — the pause a beat inserts between
/// statements is a handful of block transfers, never a whole pair. Data
/// blocks *read* per schedule beat, data blocks *written* per write beat,
/// and a cheap-entry cap so a tombstone-heavy stretch (which emits no
/// blocks) still bounds its walking and checksum work.
const MERGE_SCHEDULE_BEAT_BLOCKS: usize = 8;
const MERGE_WRITE_BEAT_BLOCKS: usize = 4;
const MERGE_BEAT_ENTRIES: usize = 64 * 1024;

impl Checkpointer {
    pub(crate) fn budget_bytes(config: &Config) -> usize {
        // One synchronous manifest/WAL client plus the fixed durable-block
        // read pool. The cache tiers draw their own budget in the constructor.
        (1 + config.object_store_get_slots) * ObjectStore::budget_bytes(config)
            + 2 * SstWriter::budget_bytes()
            + ValueIndexWriter::budget_bytes()
            + MAX_CKPT_TABLES
                * crate::storage::MAX_VALUE_ENFORCERS
                * core::mem::size_of::<ValueInstall>()
            + MANIFEST_BUF_BYTES
            + crate::store::BLOCK_SIZE
            + SST_ARENA_BYTES
            + MERGE_SCRATCH_ENTRIES * core::mem::size_of::<(SstKey, u8)>()
    }

    /// One bounded step of the paced merge — the compaction work a beat may
    /// do between statements. Starting a job, advancing its schedule scan a
    /// few blocks, streaming a few output blocks, or finishing: each is one
    /// beat, so a pair of any size merges without ever pausing the engine
    /// for more than a handful of block transfers.
    ///
    /// A job survives publishes (its pair's list positions are stable under
    /// delta appends, which only extend the tail) and is dropped when a
    /// collapse or full rewrite supersedes the pair — its blocks sweep as
    /// orphans. A crash loses only the job's progress, never data.
    fn merge_beat(&mut self, storage: &Storage) -> Result<(), SqlError> {
        let Some(mut job) = self.merge_job.take() else {
            if let Some(job) = self.merge_candidate(storage) {
                self.merge_scratch.clear();
                self.merge_writer.reset();
                let mut schema = [ColType::Bool; MAX_COLUMNS];
                let columns = storage.table(job.slot).def.schema(&mut schema);
                self.merge_writer
                    .set_pax_schema(&schema[..columns])
                    .map_err(sst_to_sql)?;
                self.merge_job = Some(job);
            }
            return Ok(());
        };
        // The published list must still hold the pair where the job left
        // it; a collapse or full rewrite replaced it, and with it the merge.
        let valid = self
            .prev_ssts
            .get(job.slot)
            .is_some_and(|list| pair_at(list, job.at) == Some((job.old0.handle, job.old1.handle)));
        if !valid {
            return Ok(());
        }
        let outcome = match job.phase {
            MergePhase::Schedule { rank, resume_lo } => {
                self.merge_schedule_beat(&mut job, rank, resume_lo)?
            }
            MergePhase::Write { cursor } => self.merge_write_beat(&mut job, cursor)?,
        };
        match outcome {
            MergeBeatOutcome::Continue => self.merge_job = Some(job),
            MergeBeatOutcome::Cancel => {}
            MergeBeatOutcome::Finished(merged) => {
                self.merge_done = Some(CompletedMerge {
                    slot: job.slot,
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
    /// skipping pairs the id scratch cannot hold (the filled-list full
    /// rewrite stays the safety net) and pairs whose scans previously
    /// overflowed it.
    fn merge_candidate(&self, storage: &Storage) -> Option<MergeJob> {
        if self.merge_job.is_some() || self.merge_done.is_some() {
            return None;
        }
        // A dirty full list cannot append its next delta while a snapshot is
        // pinned. Free one of those lists before servicing ordinary merge
        // candidates, or a smaller unrelated table can starve the publication.
        let must_free_full_list = storage.has_active_snapshots()
            && (0..storage.physical_table_count()).any(|slot| {
                storage.table(slot).live
                    && storage.table(slot).dirty
                    && self
                        .prev_ssts
                        .get(slot)
                        .is_some_and(|list| list.n == crate::storage::MAX_SPILL_SSTS)
            });
        for slot in 0..storage.physical_table_count().min(MAX_CKPT_TABLES) {
            if !storage.table(slot).live {
                continue;
            }
            let Some(list) = self.prev_ssts.get(slot) else {
                continue;
            };
            if must_free_full_list
                && (!storage.table(slot).dirty || list.n != crate::storage::MAX_SPILL_SSTS)
            {
                continue;
            }
            if list.n < MERGE_TRIGGER {
                continue;
            }
            let at = (0..list.n - 1)
                .min_by_key(|&i| {
                    list.ssts[i].expect("counted").count + list.ssts[i + 1].expect("counted").count
                })
                .expect("trigger implies at least one pair");
            let old0 = list.ssts[at].expect("counted");
            let old1 = list.ssts[at + 1].expect("counted");
            if (old0.count + old1.count) as usize > MERGE_SCRATCH_ENTRIES {
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
            || self.merge_done.is_some()
            || self.merge_candidate(storage).is_some()
    }

    pub(crate) fn maintenance_pending(&self) -> bool {
        self.published_lsn_pending_maintenance.is_some()
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
                &mut *blocks.borrow_mut(),
                &member.handle,
                resume_lo,
                MERGE_SCHEDULE_BEAT_BLOCKS,
                &mut |key, tombstone| {
                    if scratch.len() == MERGE_SCRATCH_ENTRIES {
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
    /// tombstone-heavy stretch), then suspend. Point reads ride the block
    /// cache, so a rowid-ordered walk touches each source block about once
    /// across the beats.
    fn merge_write_beat(
        &mut self,
        job: &mut MergeJob,
        cursor: usize,
    ) -> Result<MergeBeatOutcome, SqlError> {
        self.sst_arena.reset();
        let mut reader = SstReader::new(&self.sst_arena).map_err(sst_to_sql)?;
        let row_buf = self
            .sst_arena
            .alloc_slice_with(crate::store::MAX_ASSEMBLED, |_| 0u8)
            .map_err(|_| sql_err!(SQLSTATE_IO, "merge scratch exceeds the checkpoint arena"))?;
        let blocks = &self.blocks;
        let writer = &mut self.merge_writer;
        let scratch = &self.merge_scratch;
        let start_blocks = writer.roster_so_far().len();
        let mut cursor = cursor;
        let mut processed = 0usize;
        while cursor < job.schedule_len {
            if processed >= MERGE_BEAT_ENTRIES
                || writer.roster_so_far().len() - start_blocks >= MERGE_WRITE_BEAT_BLOCKS
            {
                job.phase = MergePhase::Write { cursor };
                return Ok(MergeBeatOutcome::Continue);
            }
            let (key, kind) = scratch[cursor];
            let rowid = key.rowid;
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
                        .append_tombstone_version(&mut *blocks.borrow_mut(), key)
                        .map_err(sst_to_sql)?;
                    job.count += 1;
                }
                continue;
            }
            let member = if kind & 1 == 0 { &job.old0 } else { &job.old1 };
            let len = reader
                .get_at(
                    &mut *blocks.borrow_mut(),
                    &member.handle,
                    rowid,
                    key.commit_lsn,
                    row_buf,
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
            job.crc.update(&row_buf[..len]);
            writer
                .append_version(&mut *blocks.borrow_mut(), key, &row_buf[..len])
                .map_err(sst_to_sql)?;
            job.count += 1;
        }
        if job.count == 0 {
            return Ok(MergeBeatOutcome::Finished(None));
        }
        let handle = writer
            .finish(&mut *blocks.borrow_mut())
            .map_err(sst_to_sql)?
            .ok_or_else(|| sql_err!(SQLSTATE_IO, "merge wrote rows but produced no SST"))?;
        Ok(MergeBeatOutcome::Finished(Some(PrevSst {
            handle,
            count: job.count,
            crc: job.crc.finish(),
        })))
    }

    /// Builds both clients and every fixed cache/checkpoint buffer at startup.
    /// Provider credentials and adapter selection terminate at
    /// [`crate::object_store`].
    pub(crate) fn new(config: &Config, budget: &mut Budget) -> Result<Self, CheckpointSetupError> {
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
        let blocks = std::rc::Rc::new(std::cell::RefCell::new(
            crate::store::build_tiers(budget, base, plan, cache_dir).map_err(|e| {
                CheckpointSetupError::ObjectStore(format!("block cache stack: {e:?}"))
            })?,
        ));
        Ok(Self {
            client: ObjectStore::new(config, budget)
                .map_err(|error| CheckpointSetupError::ObjectStore(error.to_string()))?,
            blocks,
            sst_arena: Arena::new(budget, "checkpoint sst", SST_ARENA_BYTES)
                .map_err(CheckpointSetupError::Budget)?,
            pending_installs: Vec::with_capacity(MAX_CKPT_TABLES),
            pending_value_installs: Vec::with_capacity(
                MAX_CKPT_TABLES * crate::storage::MAX_VALUE_ENFORCERS,
            ),
            merge_scratch: Vec::with_capacity(MERGE_SCRATCH_ENTRIES),
            roster_scratch: Vec::with_capacity(MAX_KEEP_BLOCKS),
            doomed_blocks: Vec::with_capacity(MAX_SWEEP_KEYS),
            manifest_buf: FixedBuf::new(budget, "manifest_buf", MANIFEST_BUF_BYTES)
                .map_err(CheckpointSetupError::Budget)?,
            manifest_etag: None,
            manifest_lsn: 0,
            commit_head_etag: None,
            commit_head: None,
            prev_ssts: Vec::with_capacity(MAX_CKPT_TABLES),
            referenced: Vec::with_capacity(MAX_CKPT_TABLES),
            prev_scratch: Vec::with_capacity(MAX_CKPT_TABLES),
            ref_scratch: Vec::with_capacity(MAX_CKPT_TABLES),
            doomed_scratch: Vec::with_capacity(MAX_SWEEP_KEYS),
            sweeping: false,
            published_lsn_pending_maintenance: None,
            sliced_generation: vec![0; MAX_CKPT_TABLES],
            sliced_this_sweep: vec![false; MAX_CKPT_TABLES],
            slice_writer: SstWriter::new(),
            merge_writer: SstWriter::new(),
            value_writer: ValueIndexWriter::new(),
            merge_job: None,
            merge_done: None,
            merge_turn: false,
            merge_overflow: vec![None; MAX_CKPT_TABLES],
            writer_id: crate::object_store::writer_id(config),
        })
    }

    /// The shared block stack, for the storage layer's spilled-row reader.
    pub(crate) fn block_stack(
        &self,
    ) -> std::rc::Rc<std::cell::RefCell<TieredStore<OwnedObjectStore>>> {
        std::rc::Rc::clone(&self.blocks)
    }

    pub(crate) fn enable_async_block_reads(&mut self) {
        self.blocks.borrow_mut().enable_async_gets();
    }

    pub(crate) fn disable_async_block_reads(&mut self) {
        self.blocks.borrow_mut().disable_async_gets();
    }

    pub(crate) fn block_read_slots(&self) -> usize {
        self.blocks.borrow().async_read_slots()
    }

    pub(crate) fn block_reads_busy(&self) -> bool {
        self.blocks.borrow().async_reads_busy()
    }

    pub(crate) fn pending_block_read_fd(&self, slot: usize) -> Option<std::os::fd::RawFd> {
        self.blocks.borrow().pending_read_fd(slot)
    }

    pub(crate) fn advance_pending_block_read(
        &mut self,
        slot: usize,
    ) -> Result<bool, crate::store::StoreError> {
        self.blocks.borrow_mut().advance_pending_read(slot)
    }

    pub(crate) fn next_block_read_hedge_deadline(&self) -> Option<std::time::Instant> {
        self.blocks.borrow().next_hedge_deadline()
    }

    pub(crate) fn issue_due_block_read_hedges(&mut self, now: std::time::Instant) {
        self.blocks.borrow_mut().issue_due_hedges(now);
    }

    /// Publishes one immutable committed journal batch, keyed by its first
    /// LSN.  The local journal is a cache; cold recovery obtains this tail
    /// from object storage after loading the manifest.
    pub(crate) fn publish_commit_batch(
        &mut self,
        first_lsn: u64,
        bytes: &[u8],
    ) -> Result<(), SqlError> {
        let batch = CommitBatchId::from_bytes(first_lsn, bytes);
        let key = stack_format!(
            72,
            "commits/{:020}-{:08x}.batch",
            batch.first_lsn,
            batch.digest
        );
        self.put_immutable(key.as_str(), bytes)?;
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
        self.put_immutable(descriptor_key.as_str(), descriptor.as_str().as_bytes())?;

        let mut head = StackStr::<80>::new();
        write!(
            head,
            "{}\nwriter {:016x}\nfirst {}\ndigest {:08x}\nend\n",
            COMMIT_HEAD_HEADER, self.writer_id, batch.first_lsn, batch.digest
        )
        .expect("commit head fits its fixed buffer");
        let precondition = match &self.commit_head_etag {
            Some(etag) => Precondition::IfMatch(etag),
            None => Precondition::IfNoneMatchAny,
        };
        let etag = match self
            .client
            .put(COMMIT_HEAD_KEY, head.as_str().as_bytes(), precondition)
        {
            Ok(etag) => etag,
            Err(error) if error.is_precondition_failed() => {
                let refreshed = self
                    .client
                    .get(COMMIT_HEAD_KEY, None)
                    .map_err(object_store_to_sql)?;
                let (writer, published) = parse_commit_head(self.client.body_bytes())
                    .map_err(|message| sql_err!(SQLSTATE_CAS, "{message}"))?;
                if writer != self.writer_id || published != batch {
                    return Err(sql_err!(
                        SQLSTATE_CAS,
                        "commit-head compare-and-swap failed: another writer owns this bucket"
                    ));
                }
                refreshed.etag
            }
            Err(error) => return Err(object_store_to_sql(error)),
        };
        self.commit_head_etag = Some(etag);
        self.commit_head = Some(batch);
        Ok(())
    }

    fn put_immutable(&mut self, key: &str, bytes: &[u8]) -> Result<(), SqlError> {
        match self.client.put(key, bytes, Precondition::IfNoneMatchAny) {
            Ok(_) => Ok(()),
            Err(error) if error.is_precondition_failed() => {
                self.client.get(key, None).map_err(object_store_to_sql)?;
                if self.client.body_bytes() == bytes {
                    Ok(())
                } else {
                    Err(sql_err!(
                        SQLSTATE_CAS,
                        "immutable object collision at {}",
                        key
                    ))
                }
            }
            Err(error) => Err(object_store_to_sql(error)),
        }
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
        self.doomed_scratch.clear();
        let mut batch = self.commit_head;
        while let Some(current) = batch {
            if self.doomed_scratch.len() == self.doomed_scratch.capacity() {
                return Err(CheckpointSetupError::ObjectStore(format!(
                    "commit-head chain exceeds fixed limit {}",
                    self.doomed_scratch.capacity()
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
            self.doomed_scratch.push(crate::stack_format!(
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
        for index in (0..self.doomed_scratch.len()).rev() {
            let key = self.doomed_scratch[index];
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
        scratch: &mut FixedBuf,
        mut apply: impl FnMut(u64, &[u8]) -> Result<(), SqlError>,
    ) -> Result<Option<u64>, SqlError> {
        scratch.clear();
        let mut complete_lsn = None;
        self.replay_commit_batches(floor, |lsn, record| {
            if complete_lsn.is_some() {
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

    /// Deletes commit batches whose records are entirely covered by
    /// the current manifest LSN. Called after a checkpoint.
    pub(crate) fn prune_commit_batches(&mut self, up_to_lsn: u64) -> Result<(), SqlError> {
        // Two passes because list borrows the client: collect keys into
        // pre-reserved scratch (no allocation post-freeze — this runs inside a
        // checkpoint). Keep the highest-keyed doomed segment so one straddling
        // the checkpoint boundary is never lost.
        self.doomed_scratch.clear();
        let doomed = &mut self.doomed_scratch;
        let mut overflow = false;
        let mut max_key = StackStr::<64>::new();
        self.client
            .list("commits/", |k| {
                let is_doomed = k
                    .strip_prefix("commits/")
                    .and_then(|x| x.strip_suffix(".batch"))
                    .and_then(|d| d.split_once('-'))
                    .and_then(|(first, _)| first.parse::<u64>().ok())
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
            .map_err(object_store_to_sql)?;
        for i in 0..self.doomed_scratch.len() {
            let key = self.doomed_scratch[i];
            if key.as_str() == max_key.as_str() {
                continue;
            }
            self.client
                .delete(key.as_str())
                .map_err(object_store_to_sql)?;
            let descriptor = key
                .as_str()
                .strip_suffix(".batch")
                .map(|stem| crate::stack_format!(72, "{}.head", stem))
                .expect("listed commit batch has its checked suffix");
            self.client
                .delete(descriptor.as_str())
                .map_err(object_store_to_sql)?;
        }
        if overflow {
            return Err(sql_err!(
                SQLSTATE_IO,
                "commit-batch sweep exceeds fixed limit {MAX_SWEEP_KEYS}"
            ));
        }
        Ok(())
    }

    /// Cold start: loads the manifest (if any) and rehydrates every SST
    /// into storage. Returns the manifest LSN — the WAL replay floor.
    /// Startup only (allocates freely while parsing).
    pub(crate) fn load_into(&mut self, storage: &mut Storage) -> Result<u64, CheckpointSetupError> {
        let floor = match self.client.get(MANIFEST_KEY, None) {
            Ok(r) => {
                self.manifest_etag = Some(r.etag);
                let text = core::str::from_utf8(self.client.body_bytes())
                    .map_err(|_| CheckpointSetupError::Corrupt("manifest is not UTF-8"))?
                    .to_string();
                self.load_manifest_text(storage, &text)?
            }
            Err(e) if e.is_not_found() => 0,
            Err(e) => {
                return Err(CheckpointSetupError::ObjectStore(format!(
                    "load manifest: {e}"
                )));
            }
        };
        match self.client.get(COMMIT_HEAD_KEY, None) {
            Ok(result) => {
                self.commit_head_etag = Some(result.etag);
                let (_, batch) = parse_commit_head(self.client.body_bytes())
                    .map_err(CheckpointSetupError::Corrupt)?;
                self.commit_head = Some(batch);
            }
            Err(error) if error.is_not_found() => {
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
            Err(error) => {
                return Err(CheckpointSetupError::ObjectStore(format!(
                    "load commit-head: {error}"
                )));
            }
        }
        Ok(floor)
    }

    #[inline(never)]
    fn load_manifest_text(
        &mut self,
        storage: &mut Storage,
        text: &str,
    ) -> Result<u64, CheckpointSetupError> {
        let mut lines = text.lines();
        if lines.next() != Some(MANIFEST_HEADER) {
            return Err(CheckpointSetupError::Corrupt("bad manifest header"));
        }
        let mut lsn = 0u64;
        let mut next_rowid = 1u64;
        let mut saw_large_object_allocator = false;
        // manifest table index → live slot index
        let mut slot_of: Vec<Option<usize>> = Vec::new();
        // (mindex, def, cols_seen, per-column sequence positions)
        let mut pending_def: Option<(usize, TableDef, usize, [i64; crate::storage::MAX_COLUMNS])> =
            None;
        // (mindex, list index, count, crc, handle) — the block-grid form.
        let mut bssts: Vec<(usize, usize, u64, u32, Option<SstHandle>)> = Vec::new();
        let mut value_indexes: Vec<(
            usize,
            [u16; crate::storage::MAX_INDEX_COLS],
            usize,
            ValueIndexHandle,
        )> = Vec::new();
        let mut table_statistics: Vec<(usize, crate::storage::TableStatistics)> = Vec::new();
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
                Some("lsn") => {
                    lsn = parse_field(words.next(), "lsn")?;
                }
                Some("next_rowid") => {
                    next_rowid = parse_field(words.next(), "next_rowid")?;
                }
                Some("next_lo_oid") => {
                    if saw_large_object_allocator {
                        return Err(CheckpointSetupError::Corrupt(
                            "duplicate large-object allocator",
                        ));
                    }
                    let value = words.next().ok_or(CheckpointSetupError::Corrupt(
                        "large-object allocator missing",
                    ))?;
                    let oid = if value == "-" {
                        None
                    } else {
                        Some(
                            crate::sql::ast::LargeObjectId::parse(value.parse::<u32>().map_err(
                                |_| CheckpointSetupError::Corrupt("invalid large-object allocator"),
                            )?)
                            .ok_or(CheckpointSetupError::Corrupt(
                                "invalid large-object allocator",
                            ))?,
                        )
                    };
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt(
                            "trailing large-object allocator fields",
                        ));
                    }
                    storage.restore_next_large_object_oid(oid);
                    saw_large_object_allocator = true;
                }
                Some("dbctx") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                Some("table") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    let mindex: usize = parse_field(words.next(), "table index")?;
                    let n_cols: usize = parse_field(words.next(), "table columns")?;
                    if n_cols > MAX_COLUMNS {
                        return Err(CheckpointSetupError::Corrupt("too many columns"));
                    }
                    let has_toast = parse_bool_field(words.next(), "table toast relation")?;
                    let has_rules = parse_bool_field(words.next(), "table rewrite rules")?;
                    let name = rest_of(line, 5)?;
                    let def = TableDef {
                        // The current format omits `tsch` for the public schema.
                        schema: sql_name("public")?,
                        name: sql_name(name)?,
                        columns: [empty_column(); MAX_COLUMNS],
                        n_columns: n_cols,
                        has_toast,
                        has_rules,
                        ..TableDef::empty()
                    };
                    pending_def = Some((mindex, def, 0, [0i64; crate::storage::MAX_COLUMNS]));
                }
                Some("col3") => {
                    let Some((_, def, seen, _)) = pending_def.as_mut() else {
                        return Err(CheckpointSetupError::Corrupt("col outside table"));
                    };
                    let type_code: u8 = parse_field(words.next(), "col type")?;
                    let not_null: u8 = parse_field(words.next(), "col notnull")?;
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
                    let ctype = ColType::from_code(type_code)
                        .ok_or(CheckpointSetupError::Corrupt("unknown column type code"))?;
                    let collation = crate::sql::ast::Collation::from_code(parse_field::<u8>(
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
                        Some(sql_name(&decode_hex_name(domain_hex)?)?)
                    };
                    let user_type = match (user_type_schema, domain) {
                        (None, None) => None,
                        (Some(schema), Some(name)) => {
                            Some(crate::storage::UserTypeName { schema, name })
                        }
                        _ => {
                            return Err(CheckpointSetupError::Corrupt(
                                "column user type identity is incomplete",
                            ));
                        }
                    };
                    let name = rest_of(line, 10)?;
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
                    def.columns[*seen] = ColumnMeta {
                        name: sql_name(name)?,
                        user_type,
                        ctype,
                        type_mod,
                        collation,
                        not_null: crate::storage::NotNullOrigin::from_code(not_null & 3)
                            .ok_or(CheckpointSetupError::Corrupt("invalid NOT NULL provenance"))?,
                        unique: not_null & 4 != 0,
                        primary: not_null & 8 != 0,
                        auto_increment: not_null & 16 != 0,
                        default,
                        is_identity: not_null & 64 != 0,
                        identity_always: not_null & 128 != 0,
                        auto_increment_step,
                    };
                    *seen += 1;
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
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                        values,
                    };
                    statistics.data.n_mcv += 1;
                }
                Some("tsch") => {
                    let Some((_, def, _, _)) = pending_def.as_mut() else {
                        return Err(CheckpointSetupError::Corrupt("tsch outside table"));
                    };
                    let hex = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("tsch name missing"))?;
                    def.schema = sql_name(&decode_hex_name(hex)?)?;
                }
                Some("part") => {
                    let Some((_, def, _, _)) = pending_def.as_mut() else {
                        return Err(CheckpointSetupError::Corrupt("part outside table"));
                    };
                    def.partition = parse_partition_manifest(&mut words)?;
                }
                Some("rls") => {
                    let Some((_, def, _, _)) = pending_def.as_mut() else {
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
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt("trailing rls fields"));
                    }
                }
                Some("nsp") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    let hex = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("nsp name missing"))?;
                    let name = sql_name(&decode_hex_name(hex)?)?;
                    if storage.find_schema(name.as_str()).is_none() {
                        storage.create_schema(name).map_err(|e| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest schema rejected: {}",
                                e.message.as_str()
                            ))
                        })?;
                    }
                }
                Some("lob") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    let name = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("rol name missing"))?,
                    )?;
                    let flags: u16 = parse_field(words.next(), "rol flags")?;
                    let connection_limit: i32 = parse_field(words.next(), "rol connection limit")?;
                    let salt = parse_hex_array::<16>(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("rol salt missing"))?,
                    )?;
                    let stored_key = parse_hex_array::<32>(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("rol stored key missing"))?,
                    )?;
                    let server_key = parse_hex_array::<32>(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("rol server key missing"))?,
                    )?;
                    let iterations: u32 = parse_field(words.next(), "rol iterations")?;
                    let valid_until_encoded = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("rol valid-until missing"))?;
                    let password_present = flags & (1 << 7) != 0;
                    let valid_until_present = flags & (1 << 8) != 0;
                    let password = crate::storage::RolePassword {
                        salt,
                        stored_key,
                        server_key,
                        iterations,
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
                        || (password_present && iterations == 0)
                        || (!password_present && password != crate::storage::RolePassword::EMPTY)
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
                                password: password_present.then_some(password),
                                valid_until,
                            },
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest role rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("rmem") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    let decode = |word: Option<&str>, missing: &'static str| {
                        word.ok_or(CheckpointSetupError::Corrupt(missing))
                            .and_then(decode_hex_name)
                    };
                    let role = decode(words.next(), "rmem role missing")?;
                    let member = decode(words.next(), "rmem member missing")?;
                    let grantor = decode(words.next(), "rmem grantor missing")?;
                    let flags: u8 = parse_field(words.next(), "rmem flags")?;
                    if words.next().is_some() || flags & !0x07 != 0 {
                        return Err(CheckpointSetupError::Corrupt("invalid rmem record"));
                    }
                    storage
                        .install_role_membership(
                            &role,
                            &member,
                            &grantor,
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
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                    storage
                        .install_system_setting(
                            sql_name(&name)?,
                            Some(crate::util::StackStr::from_str(&value)),
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest system setting rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("ptx") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                    let owner = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("ptx owner missing"))?,
                    )?;
                    let owner = storage
                        .find_role(&owner)
                        .ok_or(CheckpointSetupError::Corrupt("ptx owner does not exist"))?
                        as u16;
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
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                    storage.set_object_owner(object, owner, 0);
                }
                Some("acl") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                            0,
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest ACL rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("cacl") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    let class: u8 = parse_field(words.next(), "cacl class")?;
                    let class = crate::storage::AccessClass::from_u8(class)
                        .filter(|class| {
                            matches!(
                                class,
                                crate::storage::AccessClass::Table
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
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    let decode = |word: Option<&str>, missing: &'static str| {
                        word.ok_or(CheckpointSetupError::Corrupt(missing))
                            .and_then(decode_hex_name)
                    };
                    let owner = decode(words.next(), "dacl owner missing")?;
                    let schema = decode(words.next(), "dacl schema missing")?;
                    let class: u8 = parse_field(words.next(), "dacl class")?;
                    let class = crate::storage::DefaultPrivilegeClass::from_u8(class)
                        .ok_or(CheckpointSetupError::Corrupt("invalid dacl class"))?;
                    let grantee = decode(words.next(), "dacl grantee missing")?;
                    let privileges: u16 = parse_field(words.next(), "dacl privileges")?;
                    let grant_options: u16 = parse_field(words.next(), "dacl grant options")?;
                    if words.next().is_some()
                        || privileges & !class.all_privileges().0 != 0
                        || grant_options & !privileges != 0
                    {
                        return Err(CheckpointSetupError::Corrupt("invalid dacl record"));
                    }
                    let owner = storage
                        .find_role(&owner)
                        .ok_or(CheckpointSetupError::Corrupt("dacl owner does not exist"))?
                        as u16;
                    let schema = if schema.is_empty() {
                        crate::storage::DEFAULT_ACL_ALL_SCHEMAS
                    } else {
                        storage
                            .find_schema(&schema)
                            .ok_or(CheckpointSetupError::Corrupt("dacl schema does not exist"))?
                            as u16
                    };
                    let grantee = if grantee == "PUBLIC" {
                        crate::storage::PUBLIC_ROLE
                    } else {
                        storage
                            .find_role(&grantee)
                            .ok_or(CheckpointSetupError::Corrupt("dacl grantee does not exist"))?
                            as u16
                    };
                    storage
                        .change_default_acl(
                            crate::storage::DefaultAclKey {
                                owner,
                                schema,
                                class,
                                grantee,
                            },
                            true,
                            crate::storage::PrivilegeSet(privileges),
                            crate::storage::PrivilegeSet(grant_options),
                            0,
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest default ACL rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("seq") => {
                    let Some((_, _, _, serials)) = pending_def.as_mut() else {
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
                    let Some((_, def, _, _)) = pending_def.as_mut() else {
                        return Err(CheckpointSetupError::Corrupt("ukey outside table"));
                    };
                    if def.n_uniques >= crate::storage::MAX_UNIQUES {
                        return Err(CheckpointSetupError::Corrupt("too many ukey lines"));
                    }
                    let is_primary: u8 = parse_field(words.next(), "ukey primary")?;
                    let timing: u8 = parse_field(words.next(), "ukey timing")?;
                    let n_cols: usize = parse_field(words.next(), "ukey ncols")?;
                    if n_cols == 0 || n_cols > crate::storage::MAX_INDEX_COLS {
                        return Err(CheckpointSetupError::Corrupt("bad ukey ncols"));
                    }
                    let mut uk = crate::storage::UniqueKey::EMPTY;
                    uk.is_primary = is_primary != 0;
                    uk.timing = crate::storage::ConstraintTiming::from_code(timing)
                        .ok_or(CheckpointSetupError::Corrupt("bad ukey timing"))?;
                    uk.n_cols = n_cols;
                    for c in uk.columns.iter_mut().take(n_cols) {
                        *c = parse_field(words.next(), "ukey col")?;
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
                    let Some((_, def, _, _)) = pending_def.as_mut() else {
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
                    let Some((_, def, _, _)) = pending_def.as_mut() else {
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
                    let Some((_, def, _, _)) = pending_def.as_mut() else {
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
                Some("dsst") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    let mindex: usize = parse_field(words.next(), "vix table")?;
                    let n_columns: usize = parse_field(words.next(), "vix columns")?;
                    if n_columns == 0 || n_columns > crate::storage::MAX_INDEX_COLS {
                        return Err(CheckpointSetupError::Corrupt("bad vix column count"));
                    }
                    let mut columns = [0u16; crate::storage::MAX_INDEX_COLS];
                    for column in columns.iter_mut().take(n_columns) {
                        *column = parse_field(words.next(), "vix column")?;
                    }
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
                        ValueIndexHandle {
                            roster,
                            entries,
                            published_lsn,
                        },
                    ));
                }
                Some("vw6") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    load_view(storage, line)?;
                }
                Some("mv5") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    load_matview(storage, line)?;
                }
                Some("pub") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    load_publication(storage, line)?;
                }
                Some("rslot") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    load_replication_slot(storage, line)?;
                }
                Some("sub") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    load_subscription(storage, line)?;
                }
                Some("subrel") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    load_subscription_relation(storage, line)?;
                }
                Some("rtn") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    let created_at: u64 = parse_field(words.next(), "routine created_at")?;
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
                            Some(crate::storage::UserTypeName {
                                schema: sql_name(&decode_hex_name(schema)?)?,
                                name: sql_name(&decode_hex_name(name)?)?,
                            })
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
                            Some(crate::storage::UserTypeName {
                                schema: sql_name(&decode_hex_name(schema)?)?,
                                name: sql_name(&decode_hex_name(name)?)?,
                            })
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
                        crate::storage::MAX_ROUTINE_ARGUMENTS];
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
                        Some(crate::storage::UserTypeName {
                            schema: sql_name(&decode_hex_name(result_schema)?)?,
                            name: sql_name(&decode_hex_name(result_name)?)?,
                        })
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
                            if result_column_count > crate::storage::MAX_ROUTINE_ARGUMENTS {
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
                                    Some(crate::storage::UserTypeName {
                                        schema: sql_name(&decode_hex_name(schema)?)?,
                                        name: sql_name(&decode_hex_name(name)?)?,
                                    })
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
                                    created_at,
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
                    storage.commit_routine_create(slot, 0);
                }
                Some("evt") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                                source,
                                condition,
                                actions,
                                action_count: action_count as u8,
                                returning_action,
                                creation_path,
                                dependencies,
                            },
                        )
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest rewrite rule rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("cst") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    let created_at = parse_field(words.next(), "cast created_at")?;
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
                        .create_cast_from_image(crate::storage::CastDef {
                            database: crate::storage::DatabaseOid::POSTGRES,
                            created_at,
                            source,
                            target,
                            method,
                            context,
                            ddl_state: crate::storage::CatalogDdlState::Absent,
                        })
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest cast rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("tsobj") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    let created_at = parse_field(words.next(), "operator created_at")?;
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
                            created_at,
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
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                        .replay_set_operator(created_at, definition)
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest operator links rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("opf") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    let created_at = parse_field(words.next(), "operator family created_at")?;
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
                            created_at,
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
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                        if storage.operator_slot_by_oid(operator_oid, 0).is_none() {
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
                        .replay_set_operator_family(created_at, definition)
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest operator family member rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("opc") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    let created_at = parse_field(words.next(), "operator class created_at")?;
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
                            created_at,
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
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                        if storage.operator_slot_by_oid(operator, 0).is_none() {
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
                        .replay_set_operator_class(created_at, definition)
                        .map_err(|error| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest operator class member rejected: {}",
                                error.message.as_str()
                            ))
                        })?;
                }
                Some("trg") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    load_trigger(storage, line)?;
                }
                Some("trgs") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    load_partition_trigger_state(storage, line)?;
                }
                Some("pol") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    load_policy(storage, line)?;
                }
                Some("sq5") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                    let schema = read_hex(words.next(), "sq5 schema missing")?;
                    let name = read_hex(words.next(), "sq5 name missing")?;
                    let data_type: u8 = parse_field(words.next(), "sq5 type")?;
                    let increment: i64 = parse_field(words.next(), "sq5 increment")?;
                    let min_value: i64 = parse_field(words.next(), "sq5 min")?;
                    let max_value: i64 = parse_field(words.next(), "sq5 max")?;
                    let start_value: i64 = parse_field(words.next(), "sq5 start")?;
                    let cache: i64 = parse_field(words.next(), "sq5 cache")?;
                    let cycle: u8 = parse_field(words.next(), "sq5 cycle")?;
                    let last_value: i64 = parse_field(words.next(), "sq5 last")?;
                    let is_called: u8 = parse_field(words.next(), "sq5 is_called")?;
                    let log_count: i64 = parse_field(words.next(), "sq5 log_count")?;
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
                        if owner_schema.is_empty() {
                            Ok(None)
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
                    let slot = storage
                        .create_sequence(
                            sql_name(&schema)?,
                            sql_name(&name)?,
                            crate::storage::SeqSpec {
                                data_type: crate::storage::SeqType::from_u8(data_type),
                                increment,
                                min_value,
                                max_value,
                                start_value,
                                cache,
                                cycle: cycle != 0,
                            },
                            owner,
                            generator_for,
                            0,
                        )
                        .map_err(|e| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest sequence rejected: {}",
                                e.message.as_str()
                            ))
                        })?;
                    storage.commit_sequence_create(slot);
                    let seq = storage.sequence(slot);
                    seq.last_value.set(last_value);
                    seq.is_called.set(is_called != 0);
                    seq.log_count.set(log_count);
                    seq.dirty.set(false);
                }
                tag @ (Some("dom") | Some("dom2") | Some("dom3")) => {
                    let has_parent = matches!(tag, Some("dom2") | Some("dom3"));
                    let has_base_identity = tag == Some("dom3");
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                    let n_checks: usize = parse_field(words.next(), "dom nchecks")?;
                    if n_checks > crate::storage::MAX_DOMAIN_CHECKS {
                        return Err(CheckpointSetupError::Corrupt("too many domain checks"));
                    }
                    let schema = hexstr(words.next(), "dom schema missing")?;
                    let name = hexstr(words.next(), "dom name missing")?;
                    let (base_domain, base_domain_schema) = if has_parent {
                        (
                            hexstr(words.next(), "dom base domain missing")?,
                            hexstr(words.next(), "dom base domain schema missing")?,
                        )
                    } else {
                        (String::new(), String::new())
                    };
                    let (base_user_type, base_user_type_schema) = if has_base_identity {
                        (
                            hexstr(words.next(), "dom base type missing")?,
                            hexstr(words.next(), "dom base type schema missing")?,
                        )
                    } else {
                        (String::new(), String::new())
                    };
                    let default_text = hexstr(words.next(), "dom default missing")?;
                    let base = crate::sql::types::ColType::from_code(base_code)
                        .ok_or(CheckpointSetupError::Corrupt("bad domain base type"))?;
                    let mut checks =
                        [crate::storage::CheckConstraint::EMPTY; crate::storage::MAX_DOMAIN_CHECKS];
                    for check in checks.iter_mut().take(n_checks) {
                        let cname = hexstr(words.next(), "dom check name missing")?;
                        let cexpr = hexstr(words.next(), "dom check expr missing")?;
                        *check = crate::storage::CheckConstraint {
                            name: sql_name(&cname)?,
                            expression: crate::util::StackStr::from_str(&cexpr),
                            validation: crate::storage::ConstraintValidation::EnforcedValidated,
                        };
                    }
                    let base_domain = match (base_domain.is_empty(), base_domain_schema.is_empty())
                    {
                        (true, true) => None,
                        (false, false) => Some(crate::storage::UserTypeName {
                            schema: sql_name(&base_domain_schema)?,
                            name: sql_name(&base_domain)?,
                        }),
                        _ => {
                            return Err(CheckpointSetupError::Corrupt(
                                "domain parent identity is incomplete",
                            ));
                        }
                    };
                    let base_user_type =
                        match (base_user_type.is_empty(), base_user_type_schema.is_empty()) {
                            (true, true) => None,
                            (false, false) => Some(crate::storage::UserTypeName {
                                schema: sql_name(&base_user_type_schema)?,
                                name: sql_name(&base_user_type)?,
                            }),
                            _ => {
                                return Err(CheckpointSetupError::Corrupt(
                                    "domain base type identity is incomplete",
                                ));
                            }
                        };
                    let spec = crate::storage::DomainSpec {
                        base_domain,
                        base_user_type,
                        base,
                        base_type_mod,
                        not_null: not_null != 0,
                        default_expr: (!default_text.is_empty())
                            .then(|| crate::util::StackStr::from_str(&default_text)),
                        checks,
                        n_checks,
                    };
                    storage
                        .create_domain_from_manifest(sql_name(&schema)?, sql_name(&name)?, spec)
                        .map_err(|e| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest domain rejected: {}",
                                e.message.as_str()
                            ))
                        })?;
                }
                Some("enm") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                    if n_members > crate::storage::MAX_ENUM_LABELS {
                        return Err(CheckpointSetupError::Corrupt("too many enum labels"));
                    }
                    let mut members =
                        [crate::storage::EnumMember::EMPTY; crate::storage::MAX_ENUM_LABELS];
                    for member in members.iter_mut().take(n_members) {
                        let label = hexstr(words.next(), "enm label missing")?;
                        let sort_bits: u64 = parse_field(words.next(), "enm sort")?;
                        *member = crate::storage::EnumMember {
                            label: sql_name(&label)?,
                            sort: f64::from_bits(sort_bits),
                        };
                    }
                    let spec = crate::storage::EnumSpec { members, n_members };
                    storage
                        .create_enum(sql_name(&schema)?, sql_name(&name)?, spec, 0)
                        .map_err(|e| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest enum rejected: {}",
                                e.message.as_str()
                            ))
                        })?;
                }
                Some("cmp") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                        let collation_code: u8 =
                            parse_field(words.next(), "cmp field collation missing")?;
                        let collation = crate::sql::ast::Collation::from_code(collation_code)
                            .ok_or(CheckpointSetupError::Corrupt(
                                "bad composite field collation",
                            ))?;
                        let user_schema = hexstr(words.next(), "cmp field user schema missing")?;
                        let user_name = hexstr(words.next(), "cmp field user name missing")?;
                        let user_type = match (user_schema.is_empty(), user_name.is_empty()) {
                            (true, true) => None,
                            (false, false) => Some(crate::storage::UserTypeName {
                                schema: sql_name(&user_schema)?,
                                name: sql_name(&user_name)?,
                            }),
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
                    storage
                        .create_composite(
                            sql_name(&schema)?,
                            sql_name(&name)?,
                            crate::storage::CompositeSpec { fields, n_fields },
                            0,
                        )
                        .map_err(|e| {
                            CheckpointSetupError::ObjectStore(format!(
                                "manifest composite rejected: {}",
                                e.message.as_str()
                            ))
                        })?;
                }
                Some("ext") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                    let descending_mask: u16 = parse_field(words.next(), "idx descending mask")?;
                    let nulls_first_mask: u16 = parse_field(words.next(), "idx nulls-first mask")?;
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
                    let mask: u16 = parse_field(words.next(), "idx expression mask")?;
                    if mask >> n_cols != 0 {
                        return Err(CheckpointSetupError::Corrupt("bad index expression mask"));
                    };
                    for (index, expression) in expressions.iter_mut().enumerate().take(n_cols) {
                        if mask & (1 << index) != 0 {
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
                        let code: u8 = parse_field(words.next(), "idx collation")?;
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
                            Some(crate::storage::IndexOperatorClass::Builtin(
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
                            Some(crate::storage::IndexOperatorClass::Builtin(
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
                    if words.next().is_some() {
                        return Err(CheckpointSetupError::Corrupt("trailing idx fields"));
                    }
                    if descending_mask >> n_cols != 0 || nulls_first_mask >> n_cols != 0 {
                        return Err(CheckpointSetupError::Corrupt("bad index ordering mask"));
                    }
                    let mut descending = [false; crate::storage::MAX_INDEX_COLS];
                    let mut nulls_first = [false; crate::storage::MAX_INDEX_COLS];
                    for i in 0..n_cols {
                        descending[i] = descending_mask & (1 << i) != 0;
                        nulls_first[i] = nulls_first_mask & (1 << i) != 0;
                    }
                    let slot = storage
                        .create_index(
                            crate::storage::IndexDef {
                                database: crate::storage::DatabaseOid::POSTGRES,
                                created_at,
                                schema: sql_name(&schema)?,
                                name: sql_name(&name)?,
                                pending_name: None,
                                table: sql_name(&table)?,
                                ownership: crate::storage::Ownership::BOOTSTRAP,
                                columns,
                                expressions,
                                include_columns,
                                collations,
                                explicit_collations,
                                operator_classes,
                                resolved_operator_classes,
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
                                    },
                                    statistics,
                                    parent: (parent != u16::MAX).then_some(parent),
                                    kind,
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
                }
                Some("end") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    saw_end = true;
                }
                // The writer identity is CAS bookkeeping (see `writer_id`),
                // not state; the loader has no use for it.
                Some("writer") => {}
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
        if !saw_large_object_allocator {
            return Err(CheckpointSetupError::Corrupt(
                "manifest lacks large-object allocator",
            ));
        }

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
            if self.prev_ssts.len() <= slot {
                self.prev_ssts.resize(slot + 1, SlotList::EMPTY);
            }
            let expect = self.prev_ssts[slot].n;
            if let Some(handle) = handle {
                if *idx != expect {
                    return Err(CheckpointSetupError::Corrupt(
                        "dsst list index out of order",
                    ));
                }
                self.rehydrate_block_sst(storage, slot, *idx as u8, *count, handle)?;
                if !self.prev_ssts[slot].push(PrevSst {
                    handle: *handle,
                    count: *count,
                    crc: *crc,
                }) {
                    return Err(CheckpointSetupError::Corrupt(
                        "dsst list longer than the engine supports",
                    ));
                }
            }
        }
        for (slot, list) in self.prev_ssts.iter().enumerate() {
            if list.n > 0 {
                let mut handles = [None; crate::storage::MAX_SPILL_SSTS];
                let mut n = 0usize;
                for p in list.iter() {
                    handles[n] = Some(p.handle);
                    n += 1;
                }
                let handles: [SstHandle; crate::storage::MAX_SPILL_SSTS] =
                    core::array::from_fn(|i| {
                        handles[i].unwrap_or(list.ssts[0].expect("non-empty").handle)
                    });
                storage.set_spill_list(slot, &handles[..n]);
            }
        }
        // Named indexes are serialized after tables, so establish the final
        // tuple-binding shape before attaching physical generations.
        storage.rebuild_all_enforcers().map_err(|error| {
            CheckpointSetupError::ObjectStore(format!(
                "manifest value-index bindings rejected: {}",
                error.message.as_str()
            ))
        })?;
        for (mindex, columns, n_columns, handle) in value_indexes {
            let slot =
                slot_of
                    .get(mindex)
                    .copied()
                    .flatten()
                    .ok_or(CheckpointSetupError::Corrupt(
                        "vix references unknown table",
                    ))?;
            storage
                .install_value_binding(slot, &columns[..n_columns], Some(handle))
                .map_err(|error| {
                    CheckpointSetupError::ObjectStore(format!(
                        "manifest value index rejected: {}",
                        error.message.as_str()
                    ))
                })?;
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
            let n_columns = storage.table(slot).def.n_columns;
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
                    if storage.table(table).def.column_index(column.as_str()).is_none())
                })
            {
                return Err(CheckpointSetupError::Corrupt(
                    "extended statistics reference a nonexistent column",
                ));
            }
            let slot = storage
                .replay_extended_statistics(crate::storage::ExtendedStatisticsSpec {
                    created_at: statistics.created_at,
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
        sst_index: u8,
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
        let mut blocks = self.blocks.borrow_mut();
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

    /// Uploads a full snapshot and publishes it. The caller resets the WAL
    /// and compacts the heap afterwards. No-op when nothing changed.
    /// The atomic form: drives beats to completion in one call — the
    /// explicit `CHECKPOINT` statement and shutdown want to return only when
    /// the manifest is published. Returns the published LSN, `None` when
    /// there was nothing to do.
    pub(crate) fn checkpoint(
        &mut self,
        storage: &mut Storage,
        sort_scratch: &mut FixedVec<(u64, RowHome)>,
    ) -> Result<Option<u64>, SqlError> {
        loop {
            match self.checkpoint_step(storage, sort_scratch)? {
                CheckpointStep::Idle => return Ok(None),
                CheckpointStep::Working => continue,
                CheckpointStep::Published { lsn } => return Ok(Some(lsn)),
            }
        }
    }

    /// Whether a sweep is mid-flight — once true, every beat advances it
    /// until the manifest publishes, trigger conditions or not.
    pub(crate) fn sweep_active(&self) -> bool {
        self.sweeping
    }

    /// One beat of the sliced checkpoint: write one table's SSTs, or — when
    /// every table's slice is current — publish the manifest. Between beats
    /// the engine serves statements, so a checkpoint no longer stalls every
    /// connection for its whole duration; consistency holds because a table
    /// that changes after its slice ([`Table::mark_dirty`] bumps its
    /// generation) is re-sliced before the publish, and the publish itself
    /// runs only in a beat where no table has an outdated slice.
    ///
    /// A failed beat (an object-store error) leaves the sweep state where it
    /// stands; the next beat retries the same work — block writes are
    /// content-addressed, so a retry re-uploading the same bytes is free,
    /// and a crash mid-sweep leaves only orphan blocks for the next
    /// publish's garbage sweep.
    pub(crate) fn checkpoint_step(
        &mut self,
        storage: &mut Storage,
        sort_scratch: &mut FixedVec<(u64, RowHome)>,
    ) -> Result<CheckpointStep, SqlError> {
        if let Some(lsn) = self.published_lsn_pending_maintenance {
            self.collect_garbage()?;
            self.collect_block_garbage(storage)?;
            self.published_lsn_pending_maintenance = None;
            return Ok(CheckpointStep::Published { lsn });
        }
        let pinned_full_list = storage.has_active_snapshots()
            && self.merge_done.is_none()
            && (0..storage.physical_table_count()).any(|slot| {
                storage.table(slot).live
                    && storage.table(slot).dirty
                    && self
                        .prev_ssts
                        .get(slot)
                        .is_some_and(|list| list.n == crate::storage::MAX_SPILL_SSTS)
            });
        if pinned_full_list {
            if self.merge_job.is_none() && self.merge_candidate(storage).is_none() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "historical snapshot pins a full SST generation list whose merge exceeds the fixed checkpoint scratch"
                ));
            }
            self.merge_beat(storage)?;
            return Ok(CheckpointStep::Working);
        }
        // Merge beats interleave with sweep work — alternating when both
        // want the engine, so a hot sweep cannot starve compaction and a
        // long merge cannot starve publishes. A finished merge makes a
        // sweep due even at an unchanged lsn: its install needs a publish.
        let merge_due = self.merge_job.is_some()
            || (self.merge_done.is_none() && self.merge_candidate(storage).is_some());
        let sweep_due = self.sweeping
            || storage.lsn() != self.manifest_lsn
            || self.manifest_etag.is_none()
            || self.merge_done.is_some()
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
            self.sliced_this_sweep.iter_mut().for_each(|s| *s = false);
            self.pending_installs.clear();
            self.pending_value_installs.clear();
        }
        for slot in 0..storage.physical_table_count().min(MAX_CKPT_TABLES) {
            if !self.needs_slice(storage, slot) {
                continue;
            }
            let generation = storage.table(slot).generation;
            self.build_table_list(storage, sort_scratch, slot)?;
            self.sliced_generation[slot] = generation;
            self.sliced_this_sweep[slot] = true;
            return Ok(CheckpointStep::Working);
        }
        let lsn = storage.lsn();
        self.publish(storage, lsn)?;
        storage.clear_dirty_through(&self.sliced_generation);
        self.sweeping = false;
        Ok(CheckpointStep::Published { lsn })
    }

    /// Whether `slot` still needs a slice this sweep: it changed since its
    /// slice, or was never sliced while dirty. (Compaction is the merge
    /// job's business, not the sweep's.)
    fn needs_slice(&self, storage: &Storage, slot: usize) -> bool {
        let table = storage.table(slot);
        table.live && table.dirty && self.sliced_generation[slot] != table.generation
    }

    /// Assembles and publishes the manifest from the sweep's recorded
    /// per-table lists, then installs the new spill state and sweeps
    /// garbage. Runs only when no table has an outdated slice.
    fn publish(&mut self, storage: &mut Storage, lsn: u64) -> Result<(), SqlError> {
        // Delta bookkeeping collects the new per-slot references and GC
        // keep-set into pre-reserved scratch so this post-freeze path never
        // allocates.
        self.ref_scratch.clear();
        self.manifest_buf.clear();
        write_manifest(&mut self.manifest_buf, MANIFEST_HEADER)?;
        write_manifest(&mut self.manifest_buf, format_args!("lsn {lsn}"))?;
        write_manifest(
            &mut self.manifest_buf,
            format_args!("next_rowid {}", storage.peek_next_rowid()),
        )?;
        match storage.next_large_object_oid() {
            Some(oid) => write_manifest(
                &mut self.manifest_buf,
                format_args!("next_lo_oid {}", oid.get()),
            )?,
            None => write_manifest(&mut self.manifest_buf, "next_lo_oid -")?,
        }
        write_manifest(
            &mut self.manifest_buf,
            format_args!("writer {:016x}", self.writer_id),
        )?;
        let mut database_context = None;

        // Roles are durable catalog authority. Only SCRAM verifier material
        // crosses this object-backed manifest; plaintext passwords never do.
        for (_, role) in storage.live_roles() {
            use core::fmt::Write;
            let attributes = role.attributes;
            let password = attributes
                .password
                .unwrap_or(crate::storage::RolePassword::EMPTY);
            let mut name = StackStr::<130>::new();
            for byte in role.name.as_str().as_bytes() {
                let _ = write!(name, "{byte:02x}");
            }
            let mut salt = StackStr::<32>::new();
            let mut stored_key = StackStr::<64>::new();
            let mut server_key = StackStr::<64>::new();
            for byte in password.salt {
                let _ = write!(salt, "{byte:02x}");
            }
            for byte in password.stored_key {
                let _ = write!(stored_key, "{byte:02x}");
            }
            for byte in password.server_key {
                let _ = write!(server_key, "{byte:02x}");
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
                &mut self.manifest_buf,
                format_args!(
                    "rol {} {} {} {} {} {} {} {}",
                    name.as_str(),
                    flags,
                    attributes.connection_limit,
                    salt.as_str(),
                    stored_key.as_str(),
                    server_key.as_str(),
                    password.iterations,
                    valid_until.as_str()
                ),
            )?;
        }
        for (_, membership) in storage.live_role_memberships() {
            use core::fmt::Write;
            let mut role = StackStr::<130>::new();
            let mut member = StackStr::<130>::new();
            let mut grantor = StackStr::<130>::new();
            for byte in storage
                .role(membership.role as usize)
                .name
                .as_str()
                .as_bytes()
            {
                let _ = write!(role, "{byte:02x}");
            }
            for byte in storage
                .role(membership.member as usize)
                .name
                .as_str()
                .as_bytes()
            {
                let _ = write!(member, "{byte:02x}");
            }
            for byte in storage
                .role(membership.grantor as usize)
                .name
                .as_str()
                .as_bytes()
            {
                let _ = write!(grantor, "{byte:02x}");
            }
            let flags = u8::from(membership.options.admin)
                | (u8::from(membership.options.inherit) << 1)
                | (u8::from(membership.options.set) << 2);
            write_manifest(
                &mut self.manifest_buf,
                format_args!(
                    "rmem {} {} {} {}",
                    role.as_str(),
                    member.as_str(),
                    grantor.as_str(),
                    flags
                ),
            )?;
        }
        for (slot, database) in storage.databases_visible_to(0) {
            use core::fmt::Write;
            let definition = storage.database_definition(slot, 0);
            let owner = storage.object_owner(
                crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Database,
                    slot: slot as u16,
                },
                0,
            );
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
                &mut self.manifest_buf,
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
                &mut self.manifest_buf,
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
                &mut self.manifest_buf,
                format_args!("sset {} {}", name.as_str(), value.as_str()),
            )?;
        }
        for prepared in storage.prepared_transaction_catalog() {
            use core::fmt::Write;
            let mut owner = StackStr::<130>::new();
            for byte in storage
                .role_name(usize::from(prepared.owner), 0)
                .as_str()
                .as_bytes()
            {
                let _ = write!(owner, "{byte:02x}");
            }
            let mut gid = StackStr::<398>::new();
            if prepared.gid.as_str().is_empty() {
                let _ = write!(gid, "-");
            } else {
                for byte in prepared.gid.as_str().as_bytes() {
                    let _ = write!(gid, "{byte:02x}");
                }
            }
            write_manifest(
                &mut self.manifest_buf,
                format_args!(
                    "ptx {} {} {} {} {} {} {}",
                    prepared.transaction_id,
                    prepared.first_lsn,
                    prepared.prepared_lsn,
                    prepared.prepared_at,
                    prepared.database.get(),
                    owner.as_str(),
                    gid.as_str()
                ),
            )?;
        }

        // The bootstrap databases already own their built-in public schemas.
        // A user database's public schema is template data and is explicit.
        for (_, schema) in storage.checkpoint_schemas() {
            if schema.name.as_str() == "public"
                && matches!(
                    schema.database,
                    crate::storage::DatabaseOid::TEMPLATE1
                        | crate::storage::DatabaseOid::TEMPLATE0
                        | crate::storage::DatabaseOid::POSTGRES
                )
            {
                continue;
            }
            write_database_context(
                &mut self.manifest_buf,
                &mut database_context,
                schema.database,
            )?;
            use core::fmt::Write;
            let mut hex = StackStr::<130>::new();
            for b in schema.name.as_str().as_bytes() {
                let _ = write!(hex, "{b:02x}");
            }
            write_manifest(&mut self.manifest_buf, format_args!("nsp {}", hex.as_str()))?;
        }
        for (_, object) in storage.checkpoint_large_objects() {
            write_database_context(
                &mut self.manifest_buf,
                &mut database_context,
                object.database,
            )?;
            write_manifest(
                &mut self.manifest_buf,
                format_args!("lob {} {}", object.oid.get(), object.created_at),
            )?;
        }
        // Domains: `dom3 <base-code> <base-typmod> <not-null> <n-checks>
        // <hex-schema> <hex-name> <hex-base-domain> <hex-base-domain-schema>
        // <hex-base-type> <hex-base-type-schema> <hex-default>
        // [<hex-cname> <hex-cexpr>]...`. Like enums, domains precede tables
        // because generated domain-array columns bind their runtime slot while
        // the table definition is rebuilt.
        for (_, d) in storage.checkpoint_domains() {
            write_database_context(&mut self.manifest_buf, &mut database_context, d.database)?;
            use core::fmt::Write;
            let mut line = StackStr::<10_240>::new();
            let hex = |line: &mut StackStr<10_240>, s: &str| {
                if s.is_empty() {
                    let _ = write!(line, "0");
                } else {
                    for b in s.as_bytes() {
                        let _ = write!(line, "{b:02x}");
                    }
                }
            };
            let _ = write!(
                line,
                "dom3 {} {} {} {} ",
                d.base.code(),
                d.base_type_mod,
                u8::from(d.not_null),
                d.n_checks,
            );
            hex(&mut line, d.schema.as_str());
            let _ = write!(line, " ");
            hex(&mut line, d.name.as_str());
            let _ = write!(line, " ");
            hex(
                &mut line,
                d.base_domain
                    .as_ref()
                    .map(|identity| identity.name.as_str())
                    .unwrap_or(""),
            );
            let _ = write!(line, " ");
            hex(
                &mut line,
                d.base_domain
                    .as_ref()
                    .map(|identity| identity.schema.as_str())
                    .unwrap_or(""),
            );
            let _ = write!(line, " ");
            hex(
                &mut line,
                d.base_user_type
                    .as_ref()
                    .map(|identity| identity.name.as_str())
                    .unwrap_or(""),
            );
            let _ = write!(line, " ");
            hex(
                &mut line,
                d.base_user_type
                    .as_ref()
                    .map(|identity| identity.schema.as_str())
                    .unwrap_or(""),
            );
            let _ = write!(line, " ");
            hex(
                &mut line,
                d.default_expr.as_ref().map(|e| e.as_str()).unwrap_or(""),
            );
            for c in d.checks() {
                let _ = write!(line, " ");
                hex(&mut line, c.name.as_str());
                let _ = write!(line, " ");
                hex(&mut line, c.expression.as_str());
            }
            write_manifest(&mut self.manifest_buf, format_args!("{}", line.as_str()))?;
        }
        // Enum types: `enm <hex-schema> <hex-name> <n-members> [<hex-label>
        // <sort-bits>]...`. Written before tables so an enum-typed column
        // resolves its type slot when its table is rebuilt on load. The sort
        // key is emitted as its exact f64 bit pattern.
        for (_, e) in storage.checkpoint_enums() {
            write_database_context(&mut self.manifest_buf, &mut database_context, e.database)?;
            use core::fmt::Write;
            let mut line = StackStr::<10_240>::new();
            let hex = |line: &mut StackStr<10_240>, s: &str| {
                if s.is_empty() {
                    let _ = write!(line, "0");
                } else {
                    for b in s.as_bytes() {
                        let _ = write!(line, "{b:02x}");
                    }
                }
            };
            let _ = write!(line, "enm ");
            hex(&mut line, e.schema.as_str());
            let _ = write!(line, " ");
            hex(&mut line, e.name.as_str());
            let _ = write!(line, " {}", e.n_members);
            for m in e.members() {
                let _ = write!(line, " ");
                hex(&mut line, m.label.as_str());
                let _ = write!(line, " {}", m.sort.to_bits());
            }
            write_manifest(&mut self.manifest_buf, format_args!("{}", line.as_str()))?;
        }
        // Named composites precede tables because composite columns rebind by
        // catalog identity while the table definitions are restored.
        for (_, definition) in storage.checkpoint_composites() {
            write_database_context(
                &mut self.manifest_buf,
                &mut database_context,
                definition.database,
            )?;
            use core::fmt::Write;
            let mut line = StackStr::<10_240>::new();
            let hex = |line: &mut StackStr<10_240>, value: &str| {
                if value.is_empty() {
                    let _ = write!(line, "0");
                } else {
                    for byte in value.as_bytes() {
                        let _ = write!(line, "{byte:02x}");
                    }
                }
            };
            let _ = write!(line, "cmp ");
            hex(&mut line, definition.schema.as_str());
            let _ = write!(line, " ");
            hex(&mut line, definition.name.as_str());
            let _ = write!(line, " {}", definition.n_fields);
            for field in definition.fields() {
                let _ = write!(line, " ");
                hex(&mut line, field.name.as_str());
                let _ = write!(
                    line,
                    " {} {} {} {} {} {} ",
                    field.attribute_number,
                    u8::from(field.dropped),
                    u8::from(field.not_null),
                    field.ctype.code(),
                    field.type_mod,
                    field.collation.code(),
                );
                hex(
                    &mut line,
                    field
                        .user_type
                        .as_ref()
                        .map(|identity| identity.schema.as_str())
                        .unwrap_or(""),
                );
                let _ = write!(line, " ");
                hex(
                    &mut line,
                    field
                        .user_type
                        .as_ref()
                        .map(|identity| identity.name.as_str())
                        .unwrap_or(""),
                );
            }
            write_manifest(&mut self.manifest_buf, format_args!("{}", line.as_str()))?;
        }
        for slot in 0..storage.physical_table_count() {
            let table = storage.table(slot);
            if !table.live {
                // A dropped table's recorded list must not linger into the
                // GC keep-set the swap below publishes.
                if slot < self.prev_scratch.len() {
                    self.prev_scratch[slot] = SlotList::EMPTY;
                }
                continue;
            }
            write_database_context(
                &mut self.manifest_buf,
                &mut database_context,
                table.database,
            )?;
            // Table + columns into the manifest.
            write_manifest(
                &mut self.manifest_buf,
                format_args!(
                    "table {slot} {} {} {} {}",
                    table.def.n_columns,
                    u8::from(table.def.has_toast),
                    u8::from(table.def.has_rules),
                    table.def.name.as_str()
                ),
            )?;
            if table.def.schema.as_str() != "public" {
                use core::fmt::Write;
                let mut hex = StackStr::<130>::new();
                for b in table.def.schema.as_str().as_bytes() {
                    let _ = write!(hex, "{b:02x}");
                }
                write_manifest(
                    &mut self.manifest_buf,
                    format_args!("tsch {}", hex.as_str()),
                )?;
            }
            write_partition_manifest(&mut self.manifest_buf, table.def.partition)?;
            write_manifest(
                &mut self.manifest_buf,
                format_args!(
                    "rls {} {}",
                    u8::from(table.def.row_level_security.enabled),
                    u8::from(table.def.row_level_security.forced),
                ),
            )?;
            for c in table.def.columns() {
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
                let mut domain_hex = StackStr::<130>::new();
                match c.user_type {
                    Some(identity) => {
                        for b in identity.name.as_str().as_bytes() {
                            let _ = write!(domain_hex, "{b:02x}");
                        }
                    }
                    None => {
                        let _ = write!(domain_hex, "0");
                    }
                }
                write_manifest(
                    &mut self.manifest_buf,
                    format_args!(
                        "col3 {} {} {} {} {} {} {} {} {} {}",
                        c.ctype.code(),
                        flags,
                        c.type_mod,
                        default_hex.as_str(),
                        dexpr_hex.as_str(),
                        c.auto_increment_step,
                        c.collation.code(),
                        domain_schema_hex.as_str(),
                        domain_hex.as_str(),
                        c.name.as_str()
                    ),
                )?;
            }
            if table.statistics.valid {
                write_manifest(
                    &mut self.manifest_buf,
                    format_args!(
                        "stats {slot} {} {} {}",
                        table.statistics.rows,
                        table.statistics.average_row_width,
                        table.statistics.analyzed_generation
                    ),
                )?;
                for (column, statistics) in table.statistics.columns[..table.def.n_columns]
                    .iter()
                    .enumerate()
                {
                    if !statistics.valid {
                        continue;
                    }
                    write_manifest(
                        &mut self.manifest_buf,
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
            for (ci, c) in table.def.columns().iter().enumerate() {
                if c.auto_increment {
                    write_manifest(
                        &mut self.manifest_buf,
                        format_args!("seq {ci} {}", table.serial_last[ci]),
                    )?;
                }
            }
            // Constraint lines (hex-encoded names/text tolerate spaces):
            // `ukey <is_primary> <timing> <ncols> <c0..cN> <hex-name>`
            for uk in table.def.uniques() {
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
                    &mut self.manifest_buf,
                    format_args!(
                        "ukey {} {} {} {}{}",
                        u8::from(uk.is_primary),
                        uk.timing.code(),
                        uk.n_cols,
                        columns.as_str(),
                        hex_name.as_str()
                    ),
                )?;
            }
            // `chk <validation> <hex-name> <hex-predicate>`
            for check in table.def.checks() {
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
                    &mut self.manifest_buf,
                    format_args!(
                        "chk {} {} {}",
                        check.validation.code(),
                        hex_name.as_str(),
                        hexpr.as_str()
                    ),
                )?;
            }
            // `fkey <ncols> <c..> <nparent> <p..> <actions> <timing> <validation> <names>`
            for fk in table.def.fkeys() {
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
                    &mut self.manifest_buf,
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
            for exclusion in table.def.exclusions() {
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
                    &mut self.manifest_buf,
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

            // A slot not sliced this sweep carries its published list
            // forward untouched — the table is clean, so today's list is
            // yesterday's. A sliced slot's list was recorded by its beat.
            if self.prev_scratch.len() <= slot && self.prev_scratch.len() < MAX_CKPT_TABLES {
                self.prev_scratch.resize(slot + 1, SlotList::EMPTY);
            }
            if !self.sliced_this_sweep.get(slot).copied().unwrap_or(false)
                && slot < self.prev_scratch.len()
            {
                self.prev_scratch[slot] =
                    self.prev_ssts.get(slot).copied().unwrap_or(SlotList::EMPTY);
            }
            let mut new_list = self
                .prev_scratch
                .get(slot)
                .copied()
                .unwrap_or(SlotList::EMPTY);
            // A merge finished since the last publish composes here: its
            // pair still present at its position (a delta only appends at
            // the tail, so positions are stable under it) means the merged
            // member replaces the two; a collapse superseded it, and the
            // merged blocks simply sweep as orphans. Recomputed from the
            // carried base on every attempt, so a publish retried after a
            // mid-CAS failure applies it exactly once.
            if let Some(done) = &self.merge_done
                && done.slot == slot
                && pair_at(&new_list, done.at) == Some((done.old0.handle, done.old1.handle))
            {
                let mut list = SlotList::EMPTY;
                for p in new_list.iter().take(done.at) {
                    let _ = list.push(*p);
                }
                if let Some(m) = done.merged {
                    let _ = list.push(m);
                }
                for p in new_list.iter().skip(done.at + 2) {
                    let _ = list.push(*p);
                }
                self.pending_installs
                    .retain(|(s, i)| !(*s == slot && matches!(i, SlotInstall::MergePair { .. })));
                self.pending_installs.push((
                    slot,
                    SlotInstall::MergePair {
                        at: done.at,
                        handle: done.merged.map(|m| m.handle),
                    },
                ));
                new_list = list;
                if slot < self.prev_scratch.len() {
                    self.prev_scratch[slot] = new_list;
                }
            }
            for (idx, p) in new_list.iter().enumerate() {
                let h = p.handle;
                let (mut ih, mut fh, mut rh) = ([0u8; 64], [0u8; 64], [0u8; 64]);
                h.index.write_key(&mut ih);
                h.filter.write_key(&mut fh);
                h.roster.write_key(&mut rh);
                write_manifest(
                    &mut self.manifest_buf,
                    format_args!(
                        "dsst {slot} {idx} {} {} {} {} {} {}",
                        p.count,
                        p.crc,
                        core::str::from_utf8(&ih).expect("hex"),
                        core::str::from_utf8(&fh).expect("hex"),
                        core::str::from_utf8(&rh).expect("hex"),
                        if h.packed { "v3" } else { "v2" },
                    ),
                )?;
            }
            if new_list.n == 0 {
                // An empty table still records its (zero-row) state so the
                // loader creates it.
                write_manifest(
                    &mut self.manifest_buf,
                    format_args!("dsst {slot} 0 0 0 - - -"),
                )?;
            }
            for binding in 0..storage.value_binding_count(slot) {
                let (columns, n_columns) = storage.value_binding_columns(slot, binding);
                let handle = self
                    .pending_value_installs
                    .iter()
                    .find(|install| {
                        install.slot == slot
                            && install.n_columns == n_columns
                            && install.columns[..n_columns] == columns[..n_columns]
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
                    &mut self.manifest_buf,
                    format_args!(
                        "vix {slot} {n_columns} {}{} {} {}",
                        column_text.as_str(),
                        core::str::from_utf8(&roster).expect("hex"),
                        handle.entries,
                        handle.published_lsn,
                    ),
                )?;
            }
        }
        for (statistics_slot, statistics) in storage.checkpoint_extended_statistics() {
            write_database_context(
                &mut self.manifest_buf,
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
            write_manifest(&mut self.manifest_buf, format_args!("{}", line.as_str()))?;

            let data = storage.extended_statistics_data(statistics_slot, 0);
            if !data.valid {
                continue;
            }
            write_manifest(
                &mut self.manifest_buf,
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
                        &mut self.manifest_buf,
                        format_args!("estatdep {} {index} {strength}", statistics.created_at),
                    )?;
                }
            }
            for (key, column) in data.expression_statistics.iter().copied().enumerate() {
                if column.valid {
                    write_manifest(
                        &mut self.manifest_buf,
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
                    &mut self.manifest_buf,
                    format_args!(
                        "estatmcv {} {} {} {}",
                        statistics.created_at,
                        entry.hash,
                        entry.count,
                        value_hex.as_str(),
                    ),
                )?;
            }
        }
        // View SQL and names are hex because the manifest is space-separated.
        for (view_slot, view) in storage.checkpoint_views() {
            write_database_context(&mut self.manifest_buf, &mut database_context, view.database)?;
            use core::fmt::Write;
            let mut hex = StackStr::<{ 2 * crate::storage::VIEW_SQL_MAX }>::new();
            for b in storage.view_sql(view_slot).as_bytes() {
                let _ = write!(hex, "{b:02x}");
            }
            let mut hschema = StackStr::<130>::new();
            for b in view.schema.as_str().as_bytes() {
                let _ = write!(hschema, "{b:02x}");
            }
            let mut hpath = StackStr::<260>::new();
            for b in storage.view_creation_path(view_slot).as_bytes() {
                let _ = write!(hpath, "{b:02x}");
            }
            let mut hname = StackStr::<130>::new();
            for b in view.name.as_str().as_bytes() {
                let _ = write!(hname, "{b:02x}");
            }
            write_manifest(
                &mut self.manifest_buf,
                format_args!(
                    "vw6 {} {} {} {} {} {}",
                    hex.as_str(),
                    hschema.as_str(),
                    hpath.as_str(),
                    hname.as_str(),
                    u8::from(matches!(
                        view.security,
                        crate::storage::ViewSecurity::Invoker
                    )),
                    ManifestDependencies(storage.view_dependencies(view_slot))
                ),
            )?;
        }
        // Materialized views: like `vw2`, plus a trailing populated flag (0/1).
        // Publications: database-scoped names plus explicit table slots.
        for (_, publication) in storage.checkpoint_publications() {
            write_database_context(
                &mut self.manifest_buf,
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
                &mut self.manifest_buf,
                "pub {} {} {} {} {}",
                name.as_str(),
                publication.ownership.owner,
                flags,
                publication.table_count,
                publication.schema_count
            )
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "manifest exceeds its fixed buffer"
                )
            })?;
            for (index, (table, mask)) in publication.tables[..publication.table_count]
                .iter()
                .zip(&publication.table_column_masks[..publication.table_count])
                .enumerate()
            {
                let filter = publication.table_filters.get(index);
                write!(&mut self.manifest_buf, " {table} {mask} ").map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "manifest exceeds its fixed buffer"
                    )
                })?;
                if filter.is_empty() {
                    write!(&mut self.manifest_buf, "-").map_err(|_| {
                        sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "manifest exceeds its fixed buffer"
                        )
                    })?;
                } else {
                    for byte in filter.as_bytes() {
                        write!(&mut self.manifest_buf, "{byte:02x}").map_err(|_| {
                            sql_err!(
                                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                "manifest exceeds its fixed buffer"
                            )
                        })?;
                    }
                }
            }
            for schema in &publication.schemas[..publication.schema_count] {
                write!(&mut self.manifest_buf, " {schema}").map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "manifest exceeds its fixed buffer"
                    )
                })?;
            }
            writeln!(&mut self.manifest_buf).map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "manifest exceeds its fixed buffer"
                )
            })?;
        }
        // A replication slot's active flag is process-local; only its resume
        // positions survive a checkpoint and restart.
        for (_, slot) in storage.checkpoint_replication_slots() {
            write_database_context(&mut self.manifest_buf, &mut database_context, slot.database)?;
            use core::fmt::Write;
            let mut name = StackStr::<130>::new();
            for byte in slot.name.as_str().as_bytes() {
                let _ = write!(name, "{byte:02x}");
            }
            write_manifest(
                &mut self.manifest_buf,
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
                &mut self.manifest_buf,
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
                &mut self.manifest_buf,
                format_args!(
                    "sub {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}{}",
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
            for relation in storage.subscription_relations_visible_to(subscription, 0) {
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
                    &mut self.manifest_buf,
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
            write_database_context(&mut self.manifest_buf, &mut database_context, mv.database)?;
            use core::fmt::Write;
            let mut hex = StackStr::<{ 2 * crate::storage::VIEW_SQL_MAX }>::new();
            for b in mv.sql.as_str().as_bytes() {
                let _ = write!(hex, "{b:02x}");
            }
            let mut hschema = StackStr::<130>::new();
            for b in mv.schema.as_str().as_bytes() {
                let _ = write!(hschema, "{b:02x}");
            }
            let mut hpath = StackStr::<260>::new();
            for b in mv.creation_path.as_str().as_bytes() {
                let _ = write!(hpath, "{b:02x}");
            }
            let mut hname = StackStr::<130>::new();
            for b in mv.name.as_str().as_bytes() {
                let _ = write!(hname, "{b:02x}");
            }
            write_manifest(
                &mut self.manifest_buf,
                format_args!(
                    "mv5 {} {} {} {} {} {}",
                    hex.as_str(),
                    hschema.as_str(),
                    hpath.as_str(),
                    hname.as_str(),
                    u8::from(mv.populated),
                    ManifestDependencies(storage.matview_dependencies(matview_slot))
                ),
            )?;
        }
        // Sequences: hex schema/name, then the numeric parameters and the live
        // value state. A sequence stores no rows, so
        // this line is its whole durable form.
        for seq in storage.checkpoint_sequences() {
            write_database_context(&mut self.manifest_buf, &mut database_context, seq.database)?;
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
                &mut self.manifest_buf,
                format_args!(
                    "sq5 {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
                    hschema.as_str(),
                    hname.as_str(),
                    seq.data_type.to_u8(),
                    seq.increment,
                    seq.min_value,
                    seq.max_value,
                    seq.start_value,
                    seq.cache,
                    u8::from(seq.cycle),
                    seq.last_value.get(),
                    u8::from(seq.is_called.get()),
                    seq.log_count.get(),
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
                &mut self.manifest_buf,
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
            let mut arguments = StackStr::<{ crate::storage::MAX_ROUTINE_ARGUMENTS * 396 }>::new();
            for argument in routine.arguments() {
                let mut argument_name = StackStr::<130>::new();
                for byte in argument.name.as_str().as_bytes() {
                    let _ = write!(argument_name, "{byte:02x}");
                }
                if argument.name.as_str().is_empty() {
                    let _ = write!(argument_name, "-");
                }
                let _ = write!(
                    arguments,
                    " {} {}",
                    argument_name.as_str(),
                    argument.ctype.code()
                );
                if let Some(identity) = argument.user_type {
                    let mut schema = StackStr::<130>::new();
                    let mut name = StackStr::<130>::new();
                    for byte in identity.schema.as_str().as_bytes() {
                        let _ = write!(schema, "{byte:02x}");
                    }
                    for byte in identity.name.as_str().as_bytes() {
                        let _ = write!(name, "{byte:02x}");
                    }
                    let _ = write!(arguments, " {} {}", schema.as_str(), name.as_str());
                } else {
                    let _ = write!(arguments, " - -");
                }
            }
            let mut parameters = StackStr::<{ crate::storage::MAX_ROUTINE_ARGUMENTS * 660 }>::new();
            let _ = write!(parameters, " {}", routine.parameter_count);
            for parameter in routine.parameters() {
                let mut parameter_name = StackStr::<130>::new();
                for byte in parameter.name.as_str().as_bytes() {
                    let _ = write!(parameter_name, "{byte:02x}");
                }
                if parameter.name.as_str().is_empty() {
                    let _ = write!(parameter_name, "-");
                }
                let _ = write!(
                    parameters,
                    " {} {}",
                    parameter_name.as_str(),
                    parameter.ctype.code()
                );
                if let Some(identity) = parameter.user_type {
                    let mut schema = StackStr::<130>::new();
                    let mut name = StackStr::<130>::new();
                    for byte in identity.schema.as_str().as_bytes() {
                        let _ = write!(schema, "{byte:02x}");
                    }
                    for byte in identity.name.as_str().as_bytes() {
                        let _ = write!(name, "{byte:02x}");
                    }
                    let _ = write!(parameters, " {} {}", schema.as_str(), name.as_str());
                } else {
                    let _ = write!(parameters, " - -");
                }
                let _ = write!(parameters, " {} ", parameter.mode.code());
                if let Some(default) = parameter.mode.default() {
                    for byte in default.as_str().as_bytes() {
                        let _ = write!(parameters, "{byte:02x}");
                    }
                } else {
                    let _ = write!(parameters, "-");
                }
            }
            let mut configs = StackStr::<{ crate::storage::MAX_ROUTINE_CONFIGS * 390 + 8 }>::new();
            let _ = write!(configs, " {}", routine.config_count);
            for config in routine.configs() {
                let _ = write!(configs, " ");
                for byte in config.name.as_str().as_bytes() {
                    let _ = write!(configs, "{byte:02x}");
                }
                let _ = write!(configs, " ");
                if config.value.as_str().is_empty() {
                    let _ = write!(configs, "-");
                } else {
                    for byte in config.value.as_str().as_bytes() {
                        let _ = write!(configs, "{byte:02x}");
                    }
                }
            }
            let mut result_columns =
                StackStr::<{ crate::storage::MAX_ROUTINE_ARGUMENTS * 396 }>::new();
            if matches!(
                routine.kind,
                crate::storage::RoutineKind::TableFunction
                    | crate::storage::RoutineKind::RecordFunction { .. }
            ) {
                let _ = write!(result_columns, " {}", routine.result_column_count);
                for column in &routine.result_columns[..routine.result_column_count] {
                    let mut column_name = StackStr::<130>::new();
                    for byte in column.name.as_str().as_bytes() {
                        let _ = write!(column_name, "{byte:02x}");
                    }
                    if column.name.as_str().is_empty() {
                        let _ = write!(column_name, "-");
                    }
                    let _ = write!(
                        result_columns,
                        " {} {}",
                        column_name.as_str(),
                        column.ctype.code()
                    );
                    if let Some(identity) = column.user_type {
                        let mut schema = StackStr::<130>::new();
                        let mut name = StackStr::<130>::new();
                        for byte in identity.schema.as_str().as_bytes() {
                            let _ = write!(schema, "{byte:02x}");
                        }
                        for byte in identity.name.as_str().as_bytes() {
                            let _ = write!(name, "{byte:02x}");
                        }
                        let _ = write!(result_columns, " {} {}", schema.as_str(), name.as_str());
                    } else {
                        let _ = write!(result_columns, " - -");
                    }
                }
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
                for byte in identity.name.as_str().as_bytes() {
                    let _ = write!(result_name, "{byte:02x}");
                }
            } else {
                let _ = write!(result_schema, "-");
                let _ = write!(result_name, "-");
            }
            write_manifest(
                &mut self.manifest_buf,
                format_args!(
                    "rtn {} {} {} {} {} {} {}{}{}{} {} {} {} {} {} {} {} {} {} {} {} {}{} {} {}",
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
                    arguments.as_str(),
                    parameters.as_str(),
                    configs.as_str(),
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
                    result_columns.as_str(),
                    creation_path.as_str(),
                    ManifestDependencies(storage.routine_dependencies_for(slot, 0)),
                ),
            )?;
        }
        for (slot, event_trigger) in storage.checkpoint_event_triggers() {
            write_database_context(
                &mut self.manifest_buf,
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
                &mut self.manifest_buf,
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
            write_database_context(&mut self.manifest_buf, &mut database_context, rule.database)?;
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
                &mut self.manifest_buf,
                format_args!(
                    "rul {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
                    slot,
                    rule.created_at,
                    target,
                    ManifestName(schema.as_str()),
                    ManifestName(relation.as_str()),
                    ManifestName(definition.name.as_str()),
                    definition.event as u8,
                    definition.mode as u8,
                    ManifestName(definition.source.as_str()),
                    condition.start,
                    condition.len,
                    spans.as_str(),
                    definition.returning_action.map_or(u16::MAX, u16::from),
                    ManifestName(definition.creation_path.as_str()),
                    ManifestDependencies(&definition.dependencies),
                ),
            )?;
        }
        for (_, cast) in storage.checkpoint_casts() {
            write_database_context(&mut self.manifest_buf, &mut database_context, cast.database)?;
            let function = match cast.method {
                crate::storage::CastMethod::Function(oid) => oid,
                crate::storage::CastMethod::Binary | crate::storage::CastMethod::InOut => 0,
            };
            write_manifest(
                &mut self.manifest_buf,
                format_args!(
                    "cst {} {} {} {} {} {}",
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
                &mut self.manifest_buf,
                &mut database_context,
                collation.database,
            )?;
            let definition = collation.definition;
            write_manifest(
                &mut self.manifest_buf,
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
                &mut self.manifest_buf,
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
                    &mut self.manifest_buf,
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
                    &mut self.manifest_buf,
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
                    &mut self.manifest_buf,
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
                    &mut self.manifest_buf,
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
                &mut self.manifest_buf,
                &mut database_context,
                conversion.database,
            )?;
            let definition = conversion.definition;
            write_manifest(
                &mut self.manifest_buf,
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
        for (_, operator) in storage.checkpoint_operators() {
            write_database_context(
                &mut self.manifest_buf,
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
                &mut self.manifest_buf,
                format_args!(
                    "opr {} {} {} {} {} {} {} {} {} {} {} {} {}",
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
                &mut self.manifest_buf,
                &mut database_context,
                operator.database,
            )?;
            let linked_oid = |oid: Option<i32>| oid.unwrap_or(0);
            write_manifest(
                &mut self.manifest_buf,
                format_args!(
                    "oprl {} {} {}",
                    operator.oid(),
                    linked_oid(definition.commutator),
                    linked_oid(definition.negator),
                ),
            )?;
        }
        for (_, family) in storage.checkpoint_operator_families() {
            write_database_context(
                &mut self.manifest_buf,
                &mut database_context,
                family.database,
            )?;
            let definition = family.definition;
            write_manifest(
                &mut self.manifest_buf,
                format_args!(
                    "opf {} {} {} {}",
                    family.created_at,
                    ManifestName(definition.schema.as_str()),
                    ManifestName(definition.name.as_str()),
                    definition.owner,
                ),
            )?;
            for member in definition.operators.iter().filter(|member| member.used) {
                write_manifest(
                    &mut self.manifest_buf,
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
                    &mut self.manifest_buf,
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
        for (_, class) in storage.checkpoint_operator_classes() {
            write_database_context(
                &mut self.manifest_buf,
                &mut database_context,
                class.database,
            )?;
            let definition = class.definition;
            write_manifest(
                &mut self.manifest_buf,
                format_args!(
                    "opc {} {} {} {} {} {} {} {}",
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
                    &mut self.manifest_buf,
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
                    &mut self.manifest_buf,
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
        for (_, trigger) in storage.checkpoint_triggers() {
            write_database_context(
                &mut self.manifest_buf,
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
                &mut self.manifest_buf,
                format_args!(
                    "trg {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
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
        for (trigger_slot, table_slot, enabled) in storage.partition_trigger_states() {
            use core::fmt::Write;
            let trigger = storage.trigger(trigger_slot);
            write_database_context(
                &mut self.manifest_buf,
                &mut database_context,
                trigger.database,
            )?;
            let table = storage.table_def(table_slot, 0);
            let mut schema = StackStr::<130>::new();
            let mut table_name = StackStr::<130>::new();
            for byte in table.schema.as_str().as_bytes() {
                let _ = write!(schema, "{byte:02x}");
            }
            for byte in table.name.as_str().as_bytes() {
                let _ = write!(table_name, "{byte:02x}");
            }
            write_manifest(
                &mut self.manifest_buf,
                format_args!(
                    "trgs {} {} {} {}",
                    trigger.created_at,
                    schema.as_str(),
                    table_name.as_str(),
                    enabled.code(),
                ),
            )?;
        }
        for (_, policy) in storage.checkpoint_policies() {
            write_database_context(
                &mut self.manifest_buf,
                &mut database_context,
                policy.database,
            )?;
            use core::fmt::Write;
            let table = storage.table_def(usize::from(policy.table), 0);
            let definition = policy.definition_for(0);
            let mut schema = StackStr::<130>::new();
            let mut table_name = StackStr::<130>::new();
            let mut name = StackStr::<130>::new();
            let mut roles = StackStr::<1048>::new();
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
            for role in definition.roles.entries() {
                let role_name = if *role == crate::storage::PUBLIC_ROLE {
                    crate::storage::SqlName::parse("public").expect("valid role name")
                } else {
                    storage.role_name(usize::from(*role), 0)
                };
                for byte in role_name.as_str().as_bytes() {
                    let _ = write!(roles, "{byte:02x}");
                }
                let _ = roles.write_char(' ');
            }
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
            write_manifest(
                &mut self.manifest_buf,
                format_args!(
                    "pol {} {} {} {} {} {} {} {}{} {} {}",
                    policy.created_at,
                    policy.command.code(),
                    u8::from(policy.permissive),
                    schema.as_str(),
                    table_name.as_str(),
                    name.as_str(),
                    definition.roles.entries().len(),
                    roles.as_str(),
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
                    ManifestDependencies(&definition.dependencies),
                ),
            )?;
        }
        for (_, extension) in storage.checkpoint_extensions() {
            write_database_context(
                &mut self.manifest_buf,
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
            for byte in storage
                .schema_def(extension.namespace as usize)
                .name
                .as_str()
                .as_bytes()
            {
                let _ = write!(schema, "{byte:02x}");
            }
            for byte in extension.version.as_str().as_bytes() {
                let _ = write!(version, "{byte:02x}");
            }
            write_manifest(
                &mut self.manifest_buf,
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
                &mut self.manifest_buf,
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
                    &mut self.manifest_buf,
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
            if let Some(database) = comment.database {
                write_database_context(&mut self.manifest_buf, &mut database_context, database)?;
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
                &mut self.manifest_buf,
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
                &mut self.manifest_buf,
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
        for (_, index) in storage.checkpoint_indexes() {
            write_database_context(
                &mut self.manifest_buf,
                &mut database_context,
                index.database,
            )?;
            use core::fmt::Write;
            let mut columns = StackStr::<128>::new();
            for c in &index.columns[..index.n_cols] {
                let _ = write!(columns, "{c} ");
            }
            let mut includes = StackStr::<128>::new();
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
            let mut descending_mask = 0u16;
            let mut nulls_first_mask = 0u16;
            for i in 0..index.n_cols {
                descending_mask |= u16::from(index.descending[i]) << i;
                nulls_first_mask |= u16::from(index.nulls_first[i]) << i;
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
            let mut expression_mask = 0u16;
            let mut encoded_expressions = StackStr::<
                { crate::storage::MAX_INDEX_COLS * (crate::storage::INDEX_EXPRESSION_MAX * 2 + 1) },
            >::new();
            for (position, expression) in index.expressions.iter().enumerate().take(index.n_cols) {
                let Some(expression) = expression else {
                    continue;
                };
                expression_mask |= 1 << position;
                let _ = encoded_expressions.write_str(" ");
                for byte in expression.as_str().as_bytes() {
                    let _ = write!(encoded_expressions, "{byte:02x}");
                }
            }
            let mut collations = StackStr::<128>::new();
            let mut operator_classes = StackStr::<128>::new();
            let mut resolved_operator_classes = StackStr::<128>::new();
            let mut statistics = StackStr::<128>::new();
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
                    Some(crate::storage::IndexOperatorClass::Builtin(class)) => {
                        let _ = write!(operator_classes, " b{}", class.code());
                    }
                    Some(crate::storage::IndexOperatorClass::Catalog(oid)) => {
                        let _ = write!(operator_classes, " c{}", oid.get());
                    }
                }
                match index.resolved_operator_classes[position]
                    .expect("live index key has a resolved operator class")
                {
                    crate::storage::IndexOperatorClass::Builtin(class) => {
                        let _ = write!(resolved_operator_classes, " b{}", class.code());
                    }
                    crate::storage::IndexOperatorClass::Catalog(oid) => {
                        let _ = write!(resolved_operator_classes, " c{}", oid.get());
                    }
                }
                let _ = write!(statistics, " {}", index.mutable.statistics[position]);
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
            write_manifest(
                &mut self.manifest_buf,
                format_args!(
                    "idx {} {} {} {}{} {} {} {} {} {} {} {} {} {}{} {}{}{}{} {} {} {} {}{} {} {}",
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
                ),
            )?;
        }
        for (_, dependency) in storage.checkpoint_extension_dependencies() {
            write_database_context(
                &mut self.manifest_buf,
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
                &mut self.manifest_buf,
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
                &mut self.manifest_buf,
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
                &mut self.manifest_buf,
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
            if let Some(database) = storage.access_object_database(object) {
                write_database_context(&mut self.manifest_buf, &mut database_context, database)?;
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
                    &mut self.manifest_buf,
                    format_args!(
                        "own {} {} {} {} {}",
                        object.class as u8,
                        crate::storage::routine_oid(storage.routine(object.slot as usize)),
                        schema_hex.as_str(),
                        name_hex.as_str(),
                        owner_hex.as_str()
                    ),
                )
            } else {
                write_manifest(
                    &mut self.manifest_buf,
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
            if storage.table(slot).live {
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
        for (slot, _) in storage.checkpoint_sequences_with_slots() {
            write_owner(crate::storage::AccessObject {
                class: crate::storage::AccessClass::Sequence,
                slot: slot as u16,
            })?;
        }
        for (slot, _) in storage.checkpoint_schemas() {
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
            if !storage.access_object_is_live_in_catalog(acl.object) {
                continue;
            }
            if let Some(database) = storage.access_object_database(acl.object) {
                write_database_context(&mut self.manifest_buf, &mut database_context, database)?;
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
                    &mut self.manifest_buf,
                    format_args!(
                        "acl {} {} {} {} {} {} {} {}",
                        acl.object.class as u8,
                        crate::storage::routine_oid(storage.routine(acl.object.slot as usize)),
                        schema_hex.as_str(),
                        name_hex.as_str(),
                        grantee_hex.as_str(),
                        grantor_hex.as_str(),
                        acl.privileges.0,
                        acl.grant_options.0
                    ),
                )?;
            } else {
                write_manifest(
                    &mut self.manifest_buf,
                    format_args!(
                        "acl {} {} {} {} {} {} {}",
                        acl.object.class as u8,
                        schema_hex.as_str(),
                        name_hex.as_str(),
                        grantee_hex.as_str(),
                        grantor_hex.as_str(),
                        acl.privileges.0,
                        acl.grant_options.0
                    ),
                )?;
            }
        }
        for (_, acl) in storage.checkpoint_column_acls() {
            if !storage.access_object_is_live_in_catalog(acl.target.relation()) {
                continue;
            }
            write_database_context(
                &mut self.manifest_buf,
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
                &mut self.manifest_buf,
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
        for (_, acl) in storage.checkpoint_default_acls() {
            write_database_context(&mut self.manifest_buf, &mut database_context, acl.database)?;
            use core::fmt::Write;
            let owner = storage.role(acl.owner as usize).name;
            let schema = if acl.schema == crate::storage::DEFAULT_ACL_ALL_SCHEMAS {
                crate::storage::SqlName::EMPTY
            } else {
                storage.schema_def(acl.schema as usize).name
            };
            let grantee = if acl.grantee == crate::storage::PUBLIC_ROLE {
                crate::storage::SqlName::parse("PUBLIC").expect("PUBLIC fits")
            } else {
                storage.role(acl.grantee as usize).name
            };
            let mut owner_hex = StackStr::<130>::new();
            let mut schema_hex = StackStr::<130>::new();
            let mut grantee_hex = StackStr::<130>::new();
            for byte in owner.as_str().as_bytes() {
                let _ = write!(owner_hex, "{byte:02x}");
            }
            for byte in schema.as_str().as_bytes() {
                let _ = write!(schema_hex, "{byte:02x}");
            }
            for byte in grantee.as_str().as_bytes() {
                let _ = write!(grantee_hex, "{byte:02x}");
            }
            write_manifest(
                &mut self.manifest_buf,
                format_args!(
                    "dacl {} {} {} {} {} {}",
                    owner_hex.as_str(),
                    schema_hex.as_str(),
                    acl.class as u8,
                    grantee_hex.as_str(),
                    acl.privileges.0,
                    acl.grant_options.0
                ),
            )?;
        }
        write_manifest(&mut self.manifest_buf, "end")?;

        // Publish via CAS.
        let precondition = match &self.manifest_etag {
            Some(etag) => Precondition::IfMatch(etag),
            None => Precondition::IfNoneMatchAny,
        };
        let etag = match self
            .client
            .put(MANIFEST_KEY, self.manifest_buf.readable(), precondition)
        {
            Ok(etag) => etag,
            Err(e) if e.is_precondition_failed() => {
                // A previous attempt's PUT may have landed with its response
                // lost (the ambiguous failure): the bucket then holds a
                // manifest of ours under an etag this process never learned
                // — possibly an *older* one of ours, if state advanced since
                // that attempt, so byte comparison cannot recognize it. The
                // writer line can: our identity means our own write — adopt
                // its etag and republish the current state over it. Any
                // other identity is a genuine second writer, which stays a
                // loud error rather than a clobber.
                let refreshed = self
                    .client
                    .get(MANIFEST_KEY, None)
                    .map_err(object_store_to_sql)?;
                let ours = {
                    let body = self.client.body_bytes();
                    let expect = crate::stack_format!(40, "writer {:016x}", self.writer_id);
                    core::str::from_utf8(body)
                        .ok()
                        .is_some_and(|text| text.lines().any(|l| l == expect.as_str()))
                };
                if !ours {
                    return Err(sql_err!(
                        SQLSTATE_CAS,
                        "manifest compare-and-swap failed: another writer owns this bucket"
                    ));
                }
                self.client
                    .put(
                        MANIFEST_KEY,
                        self.manifest_buf.readable(),
                        Precondition::IfMatch(&refreshed.etag),
                    )
                    .map_err(object_store_to_sql)?
            }
            Err(e) => return Err(object_store_to_sql(e)),
        };
        self.manifest_etag = Some(etag);
        self.manifest_lsn = lsn;
        std::mem::swap(&mut self.prev_ssts, &mut self.prev_scratch);
        std::mem::swap(&mut self.referenced, &mut self.ref_scratch);
        // The manifest is durable: install the new spill lists (a collapse
        // remaps the table's spilled entries to slot 0) and forget the
        // flushed tombstones. A failed CAS above reaches none of this, so a
        // retry recomputes against unchanged state and the orphaned blocks
        // are swept as garbage.
        for &(slot, install) in &self.pending_installs {
            match install {
                SlotInstall::Append(h) => storage.append_spill(slot, h),
                SlotInstall::Collapse(h) => storage.collapse_spill(slot, h),
                SlotInstall::MergePair { at, handle } => storage.merge_spill_pair(slot, at, handle),
            }
            storage.clear_tombstones(slot);
        }
        self.pending_installs.clear();
        for install in &self.pending_value_installs {
            storage.install_value_binding(
                install.slot,
                &install.columns[..install.n_columns],
                install.handle,
            )?;
        }
        self.pending_value_installs.clear();
        // The completed merge is consumed with the installs — whether it
        // composed in or a collapse had superseded it, this publish settled
        // its fate either way.
        self.merge_done = None;
        // The sweep is complete the instant the installs land: everything
        // after the CAS is cleanup of the superseded generation. Marking it
        // here (not in the caller) is load-bearing — a failure below must
        // not leave the sweep active, because the swap above repurposed
        // `prev_scratch`, and a retried publish reading it would CAS a
        // manifest whose lsn claims state its lists do not carry, silently
        // shadowing every local WAL record the lsn covers.
        self.sweeping = false;

        // A successful checkpoint includes both publication and maintenance.
        // Keep the LSN until bounded sweeps finish so the next beat resumes
        // cleanup instead of falsely reporting completion.
        self.published_lsn_pending_maintenance = Some(lsn);
        self.collect_garbage()?;
        self.collect_block_garbage(storage)?;
        self.published_lsn_pending_maintenance = None;
        Ok(())
    }

    /// One beat's work for one table: computes its new SST list — carrying,
    /// delta-flushing, fully rewriting, and paying at most one paced merge —
    /// records it for the publish, and queues the storage installs that
    /// apply only after the manifest CAS lands. A re-slice (the table
    /// changed after an earlier beat of this sweep) recomputes from the
    /// published base and replaces its queued installs.
    fn build_table_list(
        &mut self,
        storage: &mut Storage,
        sort_scratch: &mut FixedVec<(u64, RowHome)>,
        slot: usize,
    ) -> Result<(), SqlError> {
        self.pending_installs.retain(|(s, _)| *s != slot);
        self.pending_value_installs
            .retain(|install| install.slot != slot);
        let mut base_list = self.prev_ssts.get(slot).copied().unwrap_or(SlotList::EMPTY);
        // A completed paced merge is part of this publish's base before a
        // dirty table decides whether its new versions fit as a delta.
        let completed = self.merge_done.as_ref().and_then(|done| {
            (done.slot == slot
                && pair_at(&base_list, done.at) == Some((done.old0.handle, done.old1.handle)))
            .then_some((done.at, done.merged))
        });
        if let Some((at, merged)) = completed {
            let mut list = SlotList::EMPTY;
            for prior in base_list.iter().take(at) {
                push_slot_list(&mut list, *prior)?;
            }
            if let Some(prior) = merged {
                push_slot_list(&mut list, prior)?;
            }
            for prior in base_list.iter().skip(at + 2) {
                push_slot_list(&mut list, *prior)?;
            }
            base_list = list;
            self.pending_installs.push((
                slot,
                SlotInstall::MergePair {
                    at,
                    handle: merged.map(|prior| prior.handle),
                },
            ));
        }
        // A clean table carries its whole SST list forward untouched.
        let clean = !storage.table(slot).dirty && base_list.n > 0;
        // A dirty table with spilled SSTs and room flushes a *delta*:
        // its heap-resident committed rows plus the tombstones recorded
        // since the last checkpoint. Otherwise it rewrites fully.
        let delta = !clean
            && storage.table(slot).dirty
            && base_list.n > 0
            && base_list.n < crate::storage::MAX_SPILL_SSTS
            && !storage.table(slot).tombstones_overflow;
        if storage.has_active_snapshots()
            && !clean
            && storage.table(slot).n_spill_ssts > 0
            && !delta
        {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "historical snapshot pins a full SST generation list for table \"{}\"",
                storage.table(slot).def.name.as_str()
            ));
        }

        let new_list: SlotList = if clean {
            base_list
        } else {
            // Collect rowids; each rowid expands to its current image plus
            // every snapshot-retained committed version. The scratch remains
            // one entry per row even when object capacity holds a long chain.
            sort_scratch.clear();
            storage.for_each_row_state(slot, &mut |rowid, state| {
                use core::ops::ControlFlow;
                let has_version = state.committed.is_some()
                    || state.committed_lsn != 0
                    || !state.history.is_empty();
                if !has_version {
                    return Ok(ControlFlow::Continue(()));
                }
                let resident = matches!(state.committed, Some(RowHome::Heap(_)))
                    || (state.committed.is_none() && state.committed_lsn != 0)
                    || (0..state.history.len()).any(|index| {
                        state.history.get(index).is_some_and(|version| {
                            version.home.is_none() || matches!(version.home, Some(RowHome::Heap(_)))
                        })
                    });
                if delta && !resident {
                    return Ok(ControlFlow::Continue(()));
                }
                let marker = state
                    .committed
                    .or_else(|| {
                        (0..state.history.len()).find_map(|index| state.history.get(index)?.home)
                    })
                    .unwrap_or(RowHome::Heap(crate::storage::RowLoc { offset: 0, len: 0 }));
                sort_scratch.push((rowid, marker)).map_err(|e| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "checkpoint scratch: {}",
                        e
                    )
                })?;
                Ok(ControlFlow::Continue(()))
            })?;
            sort_scratch
                .as_mut_slice()
                .sort_unstable_by_key(|(rowid, _)| *rowid);

            // Versions are already grouped in `(rowid, commit_lsn DESC)`
            // order. Write and checksum the same logical stream in one pass,
            // avoiding a second round of provider-neutral reads for spilled
            // images during a full rewrite.
            self.sst_arena.reset();
            self.slice_writer.reset();
            let mut schema = [ColType::Bool; MAX_COLUMNS];
            let columns = storage.table(slot).def.schema(&mut schema);
            self.slice_writer
                .set_pax_schema(&schema[..columns])
                .map_err(sst_to_sql)?;
            let writer = &mut self.slice_writer;
            let blocks = &self.blocks;
            let mut count = 0u64;
            let mut crc = Crc32c::new();
            for &(rowid, _) in sort_scratch.iter() {
                let Some(state) = storage.row_state(slot, rowid)? else {
                    continue;
                };
                let mut append_version = |commit_lsn: u64,
                                          home: Option<RowHome>|
                 -> Result<(), SqlError> {
                    if delta && home.is_some_and(|location| !matches!(location, RowHome::Heap(_))) {
                        return Ok(());
                    }
                    let key = SstKey::at(rowid, commit_lsn);
                    let mut header = [0u8; VERSIONED_SST_ENTRY_HEADER];
                    header[0..8].copy_from_slice(&rowid.to_le_bytes());
                    header[8..16].copy_from_slice(&commit_lsn.to_le_bytes());
                    header[16..20].copy_from_slice(
                        &home
                            .map_or(u32::MAX, |location| match location {
                                RowHome::Heap(row) => row.len,
                                RowHome::Spilled { len, .. } => len,
                            })
                            .to_le_bytes(),
                    );
                    crc.update(&header);
                    if let Some(location) = home {
                        storage.with_row_bytes(slot, rowid, location, |row| {
                            crc.update(row);
                            writer
                                .append_version(&mut *blocks.borrow_mut(), key, row)
                                .map_err(sst_to_sql)
                        })?
                    } else {
                        writer
                            .append_tombstone_version(&mut *blocks.borrow_mut(), key)
                            .map_err(sst_to_sql)?
                    }
                    count += 1;
                    Ok(())
                };
                if state.committed.is_some() || state.committed_lsn != 0 {
                    append_version(state.committed_lsn, state.committed)?;
                }
                for index in 0..state.history.len() {
                    if let Some(version) = state.history.get(index) {
                        append_version(version.lsn, version.home)?;
                    }
                }
            }
            let crc = crc.finish();
            let handle = writer
                .finish(&mut *blocks.borrow_mut())
                .map_err(sst_to_sql)?;

            // Storage is not touched yet: the list installs (and the
            // entry remap a collapse implies) apply only after the
            // manifest CAS lands, so a failed publish leaves memory
            // consistent with the still-current manifest.
            match (delta, handle) {
                (true, Some(h)) => {
                    let mut list = base_list;
                    if !list.push(PrevSst {
                        handle: h,
                        count,
                        crc,
                    }) {
                        return Err(sql_err!(SQLSTATE_IO, "delta flush into a full spill list"));
                    }
                    self.pending_installs.push((slot, SlotInstall::Append(h)));
                    list
                }
                (true, None) => {
                    // Dirty but nothing new to flush (e.g. the change was
                    // rolled back): the list stands.
                    base_list
                }
                (false, Some(h)) => {
                    self.pending_installs.push((slot, SlotInstall::Collapse(h)));
                    let mut list = SlotList::EMPTY;
                    push_slot_list(
                        &mut list,
                        PrevSst {
                            handle: h,
                            count,
                            crc,
                        },
                    )?;
                    list
                }
                (false, None) => SlotList::EMPTY,
            }
        };

        if self.prev_scratch.len() <= slot && self.prev_scratch.len() < MAX_CKPT_TABLES {
            self.prev_scratch.resize(slot + 1, SlotList::EMPTY);
        }
        if slot < self.prev_scratch.len() {
            self.prev_scratch[slot] = new_list;
        }
        self.build_value_indexes(storage, slot)?;
        Ok(())
    }

    /// Rebuilds each distinct constrained/named tuple as a compact key-only
    /// generation. This is deliberately a full logical rebuild: stale keys
    /// disappear at every publish, while publication remains atomic with the
    /// row generation through the same manifest CAS.
    fn build_value_indexes(&mut self, storage: &Storage, slot: usize) -> Result<(), SqlError> {
        if storage.value_binding_count(slot) == 0 {
            return Ok(());
        }
        self.sst_arena.reset();
        let key = self
            .sst_arena
            .alloc_slice_with(crate::store::MAX_PAYLOAD, |_| 0u8)
            .map_err(|_| sql_err!(SQLSTATE_IO, "persistent value-index key scratch"))?;
        let published_lsn = storage.lsn();
        for binding in 0..storage.value_binding_count(slot) {
            if !storage.value_binding_is_committed(slot, binding) {
                continue;
            }
            self.value_writer.reset();
            storage.for_each_row_state(slot, &mut |rowid, state| {
                use core::ops::ControlFlow;
                let Some(home) = state.committed else {
                    return Ok(ControlFlow::Continue(()));
                };
                let (key_len, hash) =
                    storage.encode_value_binding_key(slot, binding, rowid, home, key)?;
                self.value_writer
                    .append(
                        &mut *self.blocks.borrow_mut(),
                        hash,
                        rowid,
                        state.committed_lsn,
                        &key[..key_len],
                    )
                    .map_err(value_index_to_sql)?;
                Ok(ControlFlow::Continue(()))
            })?;
            let handle = self
                .value_writer
                .finish(&mut *self.blocks.borrow_mut(), published_lsn)
                .map_err(value_index_to_sql)?;
            let (columns, n_columns) = storage.value_binding_columns(slot, binding);
            self.pending_value_installs.push(ValueInstall {
                slot,
                columns,
                n_columns,
                handle,
            });
        }
        Ok(())
    }

    /// Mark-and-sweep over `blocks/`: the keep-set is every identity on the
    /// rosters of the SSTs the manifest just published (each roster is one
    /// block read, through the cache), plus the rosters themselves; anything
    /// else under the prefix is an orphan from a superseded checkpoint or an
    /// interrupted write, and is deleted. An undersized keep-set is a loud
    /// error rather than a successful checkpoint that silently retains debt.
    fn collect_block_garbage(&mut self, storage: &Storage) -> Result<(), SqlError> {
        self.roster_scratch.clear();
        self.sst_arena.reset();
        let scratch = self
            .sst_arena
            .alloc_slice_with(crate::store::MAX_PAYLOAD, |_| 0u8)
            .map_err(|_| sql_err!(SQLSTATE_IO, "gc scratch"))?;
        // A merge mid-flight has written blocks no published roster names
        // yet; sweeping them would destroy the job's progress.
        if self.merge_job.is_some() {
            for id in self.merge_writer.roster_so_far() {
                if self.roster_scratch.len() == MAX_KEEP_BLOCKS {
                    return Err(sql_err!(
                        SQLSTATE_IO,
                        "block GC keep-set exceeds fixed limit {MAX_KEEP_BLOCKS}"
                    ));
                }
                self.roster_scratch.push(*id);
            }
        }
        for prev in self.prev_ssts.iter().flat_map(SlotList::iter) {
            let h = prev.handle;
            if self.roster_scratch.len() + 1 > MAX_KEEP_BLOCKS {
                return Err(sql_err!(
                    SQLSTATE_IO,
                    "block GC keep-set exceeds fixed limit {MAX_KEEP_BLOCKS}"
                ));
            }
            self.roster_scratch.push(h.roster);
            let n = self
                .blocks
                .borrow_mut()
                .get(&h.roster, scratch)
                .map(|(n, _)| n)
                .map_err(|e| sql_err!(SQLSTATE_IO, "gc roster read: {:?}", e))?;
            for id_bytes in scratch[..n].chunks(32) {
                if id_bytes.len() != 32 {
                    return Err(sql_err!(
                        SQLSTATE_IO,
                        "gc roster is not a multiple of 32 bytes"
                    ));
                }
                if self.roster_scratch.len() == MAX_KEEP_BLOCKS {
                    return Err(sql_err!(
                        SQLSTATE_IO,
                        "block GC keep-set exceeds fixed limit {MAX_KEEP_BLOCKS}"
                    ));
                }
                let mut id = [0u8; 32];
                id.copy_from_slice(id_bytes);
                self.roster_scratch.push(BlockId(id));
            }
        }
        for slot in 0..storage.physical_table_count() {
            for binding in 0..storage.value_binding_count(slot) {
                let Some(handle) = storage.value_binding_handle(slot, binding) else {
                    continue;
                };
                let complete = crate::store::walk_value_roster(
                    &mut *self.blocks.borrow_mut(),
                    handle.roster,
                    scratch,
                    |id| {
                        if self.roster_scratch.len() == MAX_KEEP_BLOCKS {
                            return false;
                        }
                        self.roster_scratch.push(id);
                        true
                    },
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
                        SQLSTATE_IO,
                        "block GC keep-set exceeds fixed limit {MAX_KEEP_BLOCKS}"
                    ));
                }
            }
        }
        self.doomed_blocks.clear();
        let keep = &self.roster_scratch;
        let doomed = &mut self.doomed_blocks;
        let mut overflow = false;
        self.client
            .list("blocks/", |key| {
                let hex = key.strip_prefix("blocks/").unwrap_or(key);
                let known = parse_block_id(hex)
                    .map(|id| keep.contains(&id))
                    .unwrap_or(false);
                if !known {
                    if doomed.len() < MAX_SWEEP_KEYS {
                        doomed.push(crate::stack_format!(80, "{}", key));
                    } else {
                        overflow = true;
                    }
                }
            })
            .map_err(object_store_to_sql)?;
        for i in 0..self.doomed_blocks.len() {
            let key = self.doomed_blocks[i];
            self.client
                .delete(key.as_str())
                .map_err(object_store_to_sql)?;
        }
        if overflow {
            return Err(sql_err!(
                SQLSTATE_IO,
                "block garbage sweep exceeds fixed limit {MAX_SWEEP_KEYS}"
            ));
        }
        Ok(())
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
            .map_err(object_store_to_sql)?;
        for i in 0..self.doomed_scratch.len() {
            let key = self.doomed_scratch[i];
            self.client
                .delete(key.as_str())
                .map_err(object_store_to_sql)?;
        }
        if overflow {
            return Err(sql_err!(
                SQLSTATE_IO,
                "SST garbage sweep exceeds fixed limit {MAX_SWEEP_KEYS}"
            ));
        }
        Ok(())
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
    let bytes = hex.as_bytes();
    if bytes.len() != 2 * N {
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
    Ok(output)
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
    let packed = match words.next() {
        Some("v2") => false,
        Some("v3") => true,
        Some(_) | None => return Err(CheckpointSetupError::Corrupt("unknown dsst format")),
    };
    if words.next().is_some() {
        return Err(CheckpointSetupError::Corrupt("malformed dsst handle"));
    }
    Ok(Some(SstHandle {
        index: parse_block_id(index)?,
        filter: parse_block_id(filter)?,
        roster: parse_block_id(roster)?,
        packed,
    }))
}

fn sst_to_sql(e: crate::store::SstError) -> SqlError {
    match e {
        crate::store::SstError::Store(crate::store::StoreError::NotReady) => {
            sql_err!(
                crate::sql::eval::sqlstate::INTERNAL_IO_WAIT,
                "block fetch in progress"
            )
        }
        other => sql_err!(SQLSTATE_IO, "checkpoint sst: {:?}", other),
    }
}

fn value_index_to_sql(error: impl core::fmt::Debug) -> SqlError {
    sql_err!(SQLSTATE_IO, "persistent value-index write: {:?}", error)
}

fn write_manifest(buffer: &mut FixedBuf, line: impl core::fmt::Display) -> Result<(), SqlError> {
    use core::fmt::Write;
    writeln!(buffer, "{line}").map_err(|_| {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "manifest exceeds its fixed buffer"
        )
    })
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

fn parse_commit_head(bytes: &[u8]) -> Result<(u64, CommitBatchId), &'static str> {
    let text = core::str::from_utf8(bytes).map_err(|_| "commit-head is not UTF-8")?;
    let mut lines = text.lines();
    if lines.next() != Some(COMMIT_HEAD_HEADER) {
        return Err("bad commit-head header");
    }
    let writer = lines
        .next()
        .and_then(|line| line.strip_prefix("writer "))
        .and_then(|word| u64::from_str_radix(word, 16).ok())
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
    if hex == "-" {
        return Ok(String::new());
    }
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
    pending: Option<(usize, TableDef, usize, [i64; crate::storage::MAX_COLUMNS])>,
) -> Result<(), CheckpointSetupError> {
    if let Some((manifest_index, definition, seen, serials)) = pending {
        if seen != definition.n_columns {
            return Err(CheckpointSetupError::Corrupt(
                "manifest column count mismatch",
            ));
        }
        let slot =
            if storage
                .is_large_object_page_relation(definition.schema.as_str(), definition.name.as_str())
            {
                let slot = storage.large_object_page_table();
                let expected = &storage.table(slot).def;
                if manifest_index != slot
                    || definition.schema != expected.schema
                    || definition.name != expected.name
                    || definition.n_columns != expected.n_columns
                    || definition.has_toast != expected.has_toast
                    || definition.columns().iter().zip(expected.columns()).any(
                        |(actual, expected)| {
                            actual.name != expected.name
                                || actual.ctype != expected.ctype
                                || actual.not_null != expected.not_null
                        },
                    )
                {
                    return Err(CheckpointSetupError::Corrupt(
                        "large-object page relation definition mismatch",
                    ));
                }
                slot
            } else {
                storage.create_table(definition).map_err(|error| {
                    CheckpointSetupError::ObjectStore(format!(
                        "manifest table rejected: {}",
                        error.message.as_str()
                    ))
                })?
            };
        storage.table_mut(slot).serial_last = serials;
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
    let flags: u8 = parse_field(words.next(), "pub flags")?;
    let count: usize = parse_field(words.next(), "pub table count")?;
    let schema_count: usize = parse_field(words.next(), "pub schema count")?;
    if count > crate::storage::MAX_PUBLICATION_TABLES {
        return Err(CheckpointSetupError::Corrupt(
            "pub table count exceeds limit",
        ));
    }
    if schema_count > crate::storage::MAX_SCHEMAS {
        return Err(CheckpointSetupError::Corrupt(
            "pub schema count exceeds limit",
        ));
    }
    let mut tables = [u16::MAX; crate::storage::MAX_PUBLICATION_TABLES];
    let mut table_column_masks = [0u64; crate::storage::MAX_PUBLICATION_TABLES];
    let mut table_filter_sql =
        [crate::util::StackStr::new(); crate::storage::MAX_PUBLICATION_TABLES];
    for index in 0..count {
        tables[index] = parse_field(words.next(), "pub table")?;
        table_column_masks[index] = parse_field(words.next(), "pub table column mask")?;
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
    let mut schemas = [u8::MAX; crate::storage::MAX_SCHEMAS];
    for schema in &mut schemas[..schema_count] {
        *schema = parse_field(words.next(), "pub schema")?;
    }
    if words.next().is_some() {
        return Err(CheckpointSetupError::Corrupt("trailing pub fields"));
    }
    let slot = storage
        .create_publication(
            crate::storage::PublicationSpec {
                name: sql_name(&name)?,
                all_tables: flags & 1 != 0,
                tables: &tables[..count],
                table_column_masks: &table_column_masks[..count],
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
    let created_at = parse_field(words.next(), "policy created_at")?;
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
    if role_count == 0 || role_count > crate::storage::MAX_POLICY_ROLES {
        return Err(CheckpointSetupError::Corrupt("policy role count"));
    }
    let mut role_slots = [crate::storage::PUBLIC_ROLE; crate::storage::MAX_POLICY_ROLES];
    for role in &mut role_slots[..role_count] {
        let role_name = decode_hex_name(
            words
                .next()
                .ok_or(CheckpointSetupError::Corrupt("policy role"))?,
        )?;
        *role = if role_name.eq_ignore_ascii_case("public") {
            crate::storage::PUBLIC_ROLE
        } else {
            storage
                .find_role(&role_name)
                .ok_or(CheckpointSetupError::Corrupt("policy role does not exist"))?
                as u16
        };
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
        .restore_policy(
            created_at,
            crate::storage::PolicySpec {
                name,
                table,
                command,
                permissive,
                definition: crate::storage::PolicyDefinition {
                    roles: crate::storage::PolicyRoles::from_slice(&role_slots[..role_count])
                        .map_err(|_| CheckpointSetupError::Corrupt("policy roles"))?,
                    using,
                    with_check,
                    dependencies,
                },
            },
        )
        .map_err(|error| {
            CheckpointSetupError::ObjectStore(format!(
                "manifest policy rejected: {}",
                error.message.as_str()
            ))
        })
}

fn load_trigger(storage: &mut Storage, line: &str) -> Result<(), CheckpointSetupError> {
    let mut words = line.split_ascii_whitespace();
    if words.next() != Some("trg") {
        return Err(CheckpointSetupError::Corrupt("trigger record"));
    }
    let created_at = parse_field(words.next(), "trigger created_at")?;
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
    let update_columns: u64 = parse_field(words.next(), "trigger update columns")?;
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
        ) && update_columns != 0)
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
            created_at,
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
    let created_at: u64 = parse_field(words.next(), "partition trigger created_at")?;
    let schema = decode_hex_name(
        words
            .next()
            .ok_or(CheckpointSetupError::Corrupt("partition trigger schema"))?,
    )?;
    let table_name = decode_hex_name(
        words
            .next()
            .ok_or(CheckpointSetupError::Corrupt("partition trigger table"))?,
    )?;
    let enabled = crate::storage::TriggerEnabled::from_code(parse_field::<u8>(
        words.next(),
        "partition trigger enabled",
    )?)
    .ok_or(CheckpointSetupError::Corrupt("partition trigger enabled"))?;
    if words.next().is_some() {
        return Err(CheckpointSetupError::Corrupt(
            "malformed partition trigger state",
        ));
    }
    let trigger = storage
        .triggers_with_slots_visible_to(0)
        .find_map(|(slot, trigger)| (trigger.created_at == created_at).then_some((slot, trigger)))
        .ok_or(CheckpointSetupError::Corrupt(
            "partition trigger parent does not exist",
        ))?;
    let table = match storage.resolve_relation(Some(&schema), &table_name, 0) {
        Some(crate::storage::ResolvedRelation::Table(table)) => table,
        _ => {
            return Err(CheckpointSetupError::Corrupt(
                "partition trigger table does not exist",
            ));
        }
    };
    let crate::storage::TriggerTarget::Table(parent) = trigger.1.target else {
        return Err(CheckpointSetupError::Corrupt(
            "view trigger has partition state",
        ));
    };
    if !matches!(trigger.1.level, crate::sql::ast::TriggerLevel::Row)
        || !storage.partition_descends_from(table, usize::from(parent), 0)
    {
        return Err(CheckpointSetupError::Corrupt(
            "partition trigger state targets an unrelated table",
        ));
    }
    storage
        .restore_partition_trigger_state(trigger.0, table, enabled)
        .map_err(|error| {
            CheckpointSetupError::ObjectStore(format!(
                "manifest partition trigger rejected: {}",
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
    if enabled {
        connection
            .require_endpoint()
            .map_err(|_| CheckpointSetupError::Corrupt("enabled subscription endpoint"))?;
    }
    let slot = storage
        .create_subscription(
            crate::storage::SubscriptionSpec {
                name: sql_name(&name)?,
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
        storage.apply_subscription_advance(advance);
    }
    if cleanup {
        let dropped = storage
            .drop_subscription(&name, 0)
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
        .subscriptions_with_slots_visible_to(0)
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
fn load_view(storage: &mut Storage, line: &str) -> Result<(), CheckpointSetupError> {
    let mut words = line.split(' ');
    let _tag = words.next();
    let read_hex = |word: Option<&str>, what: &'static str| {
        word.ok_or(CheckpointSetupError::Corrupt(what))
            .and_then(decode_hex_name)
    };
    let sql = read_hex(words.next(), "vw2 sql missing")?;
    let schema = read_hex(words.next(), "vw2 schema missing")?;
    let path = read_hex(words.next(), "vw2 path missing")?;
    let name = read_hex(words.next(), "vw2 name missing")?;
    let security = match parse_field::<u8>(words.next(), "view security missing")? {
        0 => crate::storage::ViewSecurity::Definer,
        1 => crate::storage::ViewSecurity::Invoker,
        _ => return Err(CheckpointSetupError::Corrupt("invalid view security")),
    };
    let dependencies = parse_stored_query_dependencies(&mut words)?;
    use core::fmt::Write;
    let mut buffer = StackStr::<{ crate::storage::VIEW_SQL_MAX }>::new();
    let _ = write!(buffer, "{sql}");
    let mut path_buffer = StackStr::<128>::new();
    let _ = write!(path_buffer, "{path}");
    let (new_slot, old_slot) = storage
        .create_view(
            sql_name(&schema)?,
            sql_name(&name)?,
            crate::storage::StoredQueryDefinition {
                sql: buffer,
                creation_path: path_buffer,
                dependencies,
            },
            security,
            true,
            0,
        )
        .map_err(|error| {
            CheckpointSetupError::ObjectStore(format!(
                "manifest view rejected: {}",
                error.message.as_str()
            ))
        })?;
    storage.commit_view_create(new_slot);
    if let Some(old_slot) = old_slot {
        storage.commit_view_drop(old_slot);
    }
    Ok(())
}

#[inline(never)]
fn load_matview(storage: &mut Storage, line: &str) -> Result<(), CheckpointSetupError> {
    let mut words = line.split(' ');
    let _tag = words.next();
    let read_hex = |word: Option<&str>, what: &'static str| {
        word.ok_or(CheckpointSetupError::Corrupt(what))
            .and_then(decode_hex_name)
    };
    let sql = read_hex(words.next(), "mv2 sql missing")?;
    let schema = read_hex(words.next(), "mv2 schema missing")?;
    let path = read_hex(words.next(), "mv2 path missing")?;
    let name = read_hex(words.next(), "mv2 name missing")?;
    let populated: u8 = parse_field(words.next(), "mv2 populated")?;
    let dependencies = parse_stored_query_dependencies(&mut words)?;
    use core::fmt::Write;
    let mut buffer = StackStr::<{ crate::storage::VIEW_SQL_MAX }>::new();
    let _ = write!(buffer, "{sql}");
    let mut path_buffer = StackStr::<128>::new();
    let _ = write!(path_buffer, "{path}");
    let slot = storage
        .create_matview(
            sql_name(&schema)?,
            sql_name(&name)?,
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
    storage.commit_matview_create(slot);
    Ok(())
}

struct ManifestDependencies<'a>(&'a crate::storage::StoredQueryDependencies);

struct ManifestName<'a>(&'a str);

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

struct ManifestRoutineResult(crate::storage::RoutineResult);

impl core::fmt::Display for ManifestRoutineResult {
    fn fmt(&self, output: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(output, "{} ", self.0.ctype.code())?;
        match self.0.user_type {
            Some(identity) => write!(
                output,
                "{} {}",
                ManifestName(identity.schema.as_str()),
                ManifestName(identity.name.as_str())
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
            write!(output, "{} ", dependency.identity.encoded())?;
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
    let mut dependencies = crate::storage::StoredQueryDependencies::EMPTY;
    for _ in 0..count {
        let code: u8 = parse_field(words.next(), "stored-query dependency class")?;
        let class = crate::storage::DependencyClass::from_code(code).ok_or(
            CheckpointSetupError::Corrupt("unknown stored-query dependency class"),
        )?;
        let identity = crate::storage::StoredDependencyIdentity::decode(
            class,
            parse_field(words.next(), "stored-query dependency identity")?,
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
        _ => Some(crate::storage::UserTypeName {
            schema: sql_name(&decode_hex_name(schema)?)?,
            name: sql_name(&decode_hex_name(name)?)?,
        }),
    };
    Ok(crate::storage::RoutineResult { ctype, user_type })
}

fn empty_column() -> ColumnMeta {
    ColumnMeta {
        name: SqlName::parse("").expect("empty fits"),
        ctype: ColType::Bool,
        type_mod: -1,
        collation: crate::sql::ast::Collation::None,
        not_null: crate::storage::NotNullOrigin::Nullable,
        unique: false,
        primary: false,
        auto_increment: false,
        default: ColumnDefault::NONE,
        is_identity: false,
        identity_always: false,
        auto_increment_step: 1,
        user_type: None,
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
    let _ = write!(line, "part 2");
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
        let _ = write!(line, " c {}", attachment.parent);
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
    write_manifest(buffer, line.as_str())
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
    if words.next().ok_or_else(corrupt)? != "2" {
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
            Some(crate::storage::PartitionAttachment { parent, bound })
        }
        _ => return Err(corrupt()),
    };
    Ok(PartitionDef { scheme, attachment })
}

#[cfg(test)]
mod stored_dependency_tests {
    use super::*;
    use crate::mem::budget::Budget;
    use crate::mem::buffer::FixedBuf;
    use crate::storage::{DependencyClass, StoredDependencyIdentity, StoredQueryDependencies};

    #[test]
    fn dsst_manifest_accepts_only_current_complete_formats() {
        let id = "00".repeat(32);
        let mut direct = "v2".split(' ');
        assert!(
            !parse_dsst_handle(&id, &id, &id, &mut direct)
                .unwrap()
                .unwrap()
                .packed
        );
        let mut packed = "v3".split(' ');
        assert!(
            parse_dsst_handle(&id, &id, &id, &mut packed)
                .unwrap()
                .unwrap()
                .packed
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
                referenced_columns: 0b101,
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
                referenced_columns: 0,
            })
            .unwrap();
        let encoded = format!("{}", ManifestDependencies(&dependencies));
        let mut words = encoded.split(' ');
        assert_eq!(
            parse_stored_query_dependencies(&mut words).unwrap(),
            dependencies
        );
    }

    #[test]
    fn uploaded_wal_record_reconstruction_keeps_the_full_commit_frame() {
        let mut budget = Budget::new(1024);
        let mut scratch = FixedBuf::new(&mut budget, "replication scratch", 128).unwrap();
        let record = [37, 0, 0, 0, 0, 0, 0, 0, 9, 0, 0, 0];
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
            Some(crate::wal::WalOp::Commit { transaction_id: 9 })
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
}
