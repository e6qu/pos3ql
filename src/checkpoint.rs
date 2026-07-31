//! Checkpointing: the durable home of the database is the bucket.
//!
//! A checkpoint writes every live table as a block-granular SST — sorted
//! data blocks, a sparse index, a bloom filter and a roster, all
//! content-addressed objects under `blocks/` — through the tiered cache
//! stack (`block_cache_bytes` RAM frames over a `disk_cache_bytes` slot
//! file), then publishes a `manifest` object naming each SST's root blocks
//! via compare-and-swap (`If-Match` on the previous ETag, `If-None-Match: *`
//! for the first). After the manifest lands, unreferenced blocks are swept
//! (each SST enumerable by its one roster block), the WAL restarts, and the
//! row heap is compacted. A node with an empty disk cold-starts by loading
//! the manifest, scanning each SST block-wise through the same cache, and
//! replaying whatever WAL tail is newer than the manifest's LSN. Manifests
//! from before the block grid (whole-object `sst/` entries) still load; the
//! next checkpoint rewrites them as block SSTs and sweeps the old objects.
//!
//! CAS on the manifest means a second writer pointed at the same bucket
//! fails loudly instead of corrupting anything.

use crate::config::Config;
use crate::mem::arena::Arena;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::buffer::FixedBuf;
use crate::mem::fixed_vec::FixedVec;
use crate::object_store::{Client as ObjectStore, Error as ObjectError, Precondition};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::types::ColType;
use crate::sql_err;
use crate::stack_format;
use crate::storage::{ColumnMeta, MAX_COLUMNS, OwnedDatum, RowHome, SqlName, Storage, TableDef};
use crate::store::{
    BlockId, BlockStore, OwnedObjectStore, SstHandle, SstKey, SstReader, SstWriter, StackPlan,
    TieredStore, ValueIndexHandle, ValueIndexWriter,
};
use crate::util::StackStr;
use crate::wal::crc32c::Crc32c;

pub(crate) const MANIFEST_KEY: &str = "manifest";
const MANIFEST_HEADER: &str = "pos3ql-manifest-v2";
const MANIFEST_BUF_BYTES: usize = 256 * 1024;
const SST_MAGIC: u64 = 0x3154_5353_4c51_3350; // "P3QLSST1" little-endian
const SST_FOOTER_LEN: usize = 20; // count u64 | crc u32 | magic u64
const SST_ENTRY_HEADER: usize = 12; // rowid u64 | len u32
const VERSIONED_SST_ENTRY_HEADER: usize = 20; // rowid u64 | commit_lsn u64 | len u32

/// io_error — object storage trouble surfaced to a statement.
const SQLSTATE_IO: &str = "58030";
/// serialization_failure — manifest CAS lost to another writer.
const SQLSTATE_CAS: &str = "40001";

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
    client: ObjectStore,
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
    manifest_etag: Option<StackStr<80>>,
    manifest_lsn: u64,
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
/// touches the allocator. A sweep that would exceed these logs and defers
/// the remainder to the next checkpoint.
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
/// exceeds it skips its merge that cycle (the full-rewrite fallback at a
/// filled list stays the safety net).
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
        // Two clients: one for manifest/WAL objects, one inside the block
        // stack. The cache tiers draw their own budget in the constructor;
        // this accounts the fixed parts.
        2 * ObjectStore::budget_bytes(config)
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
            && (0..storage.table_count()).any(|slot| {
                storage.table(slot).live
                    && storage.table(slot).dirty
                    && self
                        .prev_ssts
                        .get(slot)
                        .is_some_and(|list| list.n == crate::storage::MAX_SPILL_SSTS)
            });
        for slot in 0..storage.table_count().min(MAX_CKPT_TABLES) {
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
        let block_client = ObjectStore::new(config, budget)
            .map_err(|error| CheckpointSetupError::ObjectStore(error.to_string()))?;
        let base = OwnedObjectStore::new(block_client, "blocks/");
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
            prev_ssts: Vec::with_capacity(MAX_CKPT_TABLES),
            referenced: Vec::with_capacity(MAX_CKPT_TABLES),
            prev_scratch: Vec::with_capacity(MAX_CKPT_TABLES),
            ref_scratch: Vec::with_capacity(MAX_CKPT_TABLES),
            doomed_scratch: Vec::with_capacity(MAX_SWEEP_KEYS),
            sweeping: false,
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

    /// Uploads a committed WAL batch as a segment keyed by its first LSN,
    /// so a lost-disk cold start can replay everything past the manifest.
    /// Called with the raw journal bytes of one commit.
    pub(crate) fn upload_wal_segment(
        &mut self,
        first_lsn: u64,
        bytes: &[u8],
    ) -> Result<(), SqlError> {
        let key = stack_format!(48, "wal/{:020}.seg", first_lsn);
        self.client
            .put(key.as_str(), bytes, Precondition::None)
            .map_err(object_store_to_sql)?;
        Ok(())
    }

    /// Downloads and replays WAL segments with records past `floor`, in
    /// ascending order, feeding each record to `apply`. The caller merges
    /// these with the local journal's records by LSN before applying:
    /// neither source alone spans the committed history (the journal may
    /// restart mid-history after a disk wipe or end early at a torn write,
    /// and the segments lack whatever a failed upload left journaled-only).
    /// Startup only (allocates while listing/parsing).
    pub(crate) fn replay_wal_segments(
        &mut self,
        floor: u64,
        mut apply: impl FnMut(u64, &[u8]) -> Result<(), SqlError>,
    ) -> Result<(), CheckpointSetupError> {
        let mut keys: Vec<String> = Vec::new();
        self.client
            .list("wal/", |k| keys.push(k.to_string()))
            .map_err(|e| CheckpointSetupError::ObjectStore(format!("list wal: {e}")))?;
        keys.sort();
        for key in &keys {
            // Key is wal/<20-digit first lsn>.seg
            let Some(digits) = key
                .strip_prefix("wal/")
                .and_then(|k| k.strip_suffix(".seg"))
            else {
                continue;
            };
            let Ok(_first_lsn) = digits.parse::<u64>() else {
                continue;
            };
            // Ranged, buffer-sized windows: a segment is one committed WAL
            // batch, whose size is bounded by wal_buffer_bytes — which may
            // exceed the response buffer. An unranged GET would upload fine
            // and then be unrecoverable at cold start (ResponseTooLarge), so
            // the segment streams through the buffer instead; a partially
            // fetched record re-fetches from its own start.
            let mut offset = 0u64;
            loop {
                let to = offset + self.client.response_capacity() as u64 - 1;
                match self.client.get(key, Some((offset, to))) {
                    Ok(_) => {}
                    // Past the end of the object: the segment is fully read.
                    Err(ObjectError::Status { code: 416, .. }) => break,
                    Err(e) => {
                        return Err(CheckpointSetupError::ObjectStore(format!(
                            "get wal segment: {e}"
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
                        "wal record in {key} exceeds object_store_response_bytes; raise it past wal_buffer_bytes"
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

    /// Deletes uploaded WAL segments whose records are entirely covered by
    /// the current manifest LSN. Called after a checkpoint.
    pub(crate) fn prune_wal_segments(&mut self, up_to_lsn: u64) -> Result<(), SqlError> {
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
            .map_err(object_store_to_sql)?;
        for i in 0..self.doomed_scratch.len() {
            let key = self.doomed_scratch[i];
            if key.as_str() == max_key.as_str() {
                continue;
            }
            self.client
                .delete(key.as_str())
                .map_err(object_store_to_sql)?;
        }
        if overflow {
            eprintln!("pos3ql: wal segments exceed one sweep; continuing next checkpoint");
        }
        Ok(())
    }

    /// Cold start: loads the manifest (if any) and rehydrates every SST
    /// into storage. Returns the manifest LSN — the WAL replay floor.
    /// Startup only (allocates freely while parsing).
    pub(crate) fn load_into(&mut self, storage: &mut Storage) -> Result<u64, CheckpointSetupError> {
        match self.client.get(MANIFEST_KEY, None) {
            Ok(r) => {
                self.manifest_etag = Some(r.etag);
            }
            Err(e) if e.is_not_found() => return Ok(0),
            Err(e) => {
                return Err(CheckpointSetupError::ObjectStore(format!(
                    "load manifest: {e}"
                )));
            }
        }
        let text = core::str::from_utf8(self.client.body_bytes())
            .map_err(|_| CheckpointSetupError::Corrupt("manifest is not UTF-8"))?
            .to_string();
        self.load_manifest_text(storage, &text)
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
        // manifest table index → live slot index
        let mut slot_of: Vec<Option<usize>> = Vec::new();
        // (mindex, def, cols_seen, per-column sequence positions)
        let mut pending_def: Option<(usize, TableDef, usize, [i64; crate::storage::MAX_COLUMNS])> =
            None;
        let mut ssts: Vec<(String, usize, u64, u64, u32)> = Vec::new();
        // (mindex, list index, count, crc, handle) — the block-grid form.
        let mut bssts: Vec<(usize, usize, u64, u32, Option<SstHandle>)> = Vec::new();
        let mut value_indexes: Vec<(
            usize,
            [u16; crate::storage::MAX_INDEX_COLS],
            usize,
            ValueIndexHandle,
        )> = Vec::new();
        let mut table_statistics: Vec<(usize, crate::storage::TableStatistics)> = Vec::new();
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
                Some("table") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    let mindex: usize = parse_field(words.next(), "table index")?;
                    let n_cols: usize = parse_field(words.next(), "table columns")?;
                    if n_cols > MAX_COLUMNS {
                        return Err(CheckpointSetupError::Corrupt("too many columns"));
                    }
                    let name = rest_of(line, 3)?;
                    let def = TableDef {
                        // `tsch` (written right after) overrides; a manifest
                        // from before schemas existed has none.
                        schema: sql_name("public")?,
                        name: sql_name(name)?,
                        columns: [empty_column(); MAX_COLUMNS],
                        n_columns: n_cols,
                        ..TableDef::empty()
                    };
                    pending_def = Some((mindex, def, 0, [0i64; crate::storage::MAX_COLUMNS]));
                }
                tag @ (Some("col") | Some("col2")) => {
                    let has_user_type_schema = tag == Some("col2");
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
                    let user_type_schema = if has_user_type_schema {
                        let schema_hex = words.next().ok_or(CheckpointSetupError::Corrupt(
                            "col user type schema missing",
                        ))?;
                        if schema_hex == "0" {
                            None
                        } else {
                            Some(sql_name(&decode_hex_name(schema_hex)?)?)
                        }
                    } else {
                        None
                    };
                    let domain_hex = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("col domain missing"))?;
                    let domain = if domain_hex == "0" {
                        None
                    } else {
                        Some(sql_name(&decode_hex_name(domain_hex)?)?)
                    };
                    let name = rest_of(line, if has_user_type_schema { 9 } else { 8 })?;
                    if *seen >= def.n_columns {
                        return Err(CheckpointSetupError::Corrupt("too many col lines"));
                    }
                    def.columns[*seen] = ColumnMeta {
                        name: sql_name(name)?,
                        domain,
                        ctype: ColType::from_code(type_code)
                            .ok_or(CheckpointSetupError::Corrupt("unknown column type code"))?,
                        type_mod,
                        not_null: not_null & 1 != 0,
                        unique: not_null & 2 != 0,
                        primary: not_null & 4 != 0,
                        auto_increment: not_null & 8 != 0,
                        default_value: default_from_hex(default_hex)?,
                        default_expr,
                        is_generated: not_null & 16 != 0,
                        is_identity: not_null & 32 != 0,
                        identity_always: not_null & 64 != 0,
                        auto_increment_step,
                        user_type_schema,
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
                Some("tsch") => {
                    let Some((_, def, _, _)) = pending_def.as_mut() else {
                        return Err(CheckpointSetupError::Corrupt("tsch outside table"));
                    };
                    let hex = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("tsch name missing"))?;
                    def.schema = sql_name(&decode_hex_name(hex)?)?;
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
                    let valid_until = match words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("rol valid-until missing"))?
                    {
                        "-" => String::new(),
                        "0" => String::new(),
                        encoded => decode_hex_name(encoded)?,
                    };
                    if words.next().is_some()
                        || flags & !0x01ff != 0
                        || valid_until.len() > crate::storage::ROLE_VALID_UNTIL_MAX
                        || (flags & (1 << 7) != 0 && iterations == 0)
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
                                password: crate::storage::RolePassword {
                                    salt,
                                    stored_key,
                                    server_key,
                                    iterations,
                                },
                                has_password: flags & (1 << 7) != 0,
                                valid_until: crate::util::StackStr::from_str(&valid_until),
                                has_valid_until: flags & (1 << 8) != 0,
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
                Some("own") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    let class: u8 = parse_field(words.next(), "own class")?;
                    let class = crate::storage::AccessClass::from_u8(class)
                        .ok_or(CheckpointSetupError::Corrupt("invalid own class"))?;
                    let schema = decode_hex_name(
                        words
                            .next()
                            .ok_or(CheckpointSetupError::Corrupt("own schema missing"))?,
                    )?;
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
                    let object = storage
                        .resolve_access_object(class, &schema, &name, 0)
                        .ok_or(CheckpointSetupError::Corrupt("own target does not exist"))?;
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
                        || privileges & !0x07ff != 0
                        || grant_options & !privileges != 0
                    {
                        return Err(CheckpointSetupError::Corrupt("invalid acl record"));
                    }
                    let object = storage
                        .resolve_access_object(class, &schema, &name, 0)
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
                        || privileges & !0x07ff != 0
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
                            .ok_or(CheckpointSetupError::Corrupt(
                                "dacl schema does not exist",
                            ))? as u16
                    };
                    let grantee = if grantee == "PUBLIC" {
                        crate::storage::PUBLIC_ROLE
                    } else {
                        storage
                            .find_role(&grantee)
                            .ok_or(CheckpointSetupError::Corrupt(
                                "dacl grantee does not exist",
                            ))? as u16
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
                    let n_cols: usize = parse_field(words.next(), "ukey ncols")?;
                    if n_cols == 0 || n_cols > crate::storage::MAX_INDEX_COLS {
                        return Err(CheckpointSetupError::Corrupt("bad ukey ncols"));
                    }
                    let mut uk = crate::storage::UniqueKey::EMPTY;
                    uk.is_primary = is_primary != 0;
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
                    let hex_name = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("chk name missing"))?;
                    let hexpr = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("chk expression missing"))?;
                    let mut check = crate::storage::CheckConstraint::EMPTY;
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
                    fk.on_delete = crate::storage::FkAction::from_code(od)
                        .ok_or(CheckpointSetupError::Corrupt("bad fkey on_delete"))?;
                    fk.on_update = crate::storage::FkAction::from_code(ou)
                        .ok_or(CheckpointSetupError::Corrupt("bad fkey on_update"))?;
                    let hex_name = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("fkey name missing"))?;
                    let hparent = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("fkey parent missing"))?;
                    fk.name = sql_name(&decode_hex_name(hex_name)?)?;
                    fk.parent = sql_name(&decode_hex_name(hparent)?)?;
                    fk.parent_schema = match words.next() {
                        Some(hex) => sql_name(&decode_hex_name(hex)?)?,
                        None => sql_name("public")?,
                    };
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
                Some("bsst") => {
                    // The single-SST form from before delta flushes: list
                    // index 0 by construction.
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    let mindex: usize = parse_field(words.next(), "bsst table")?;
                    let count: u64 = parse_field(words.next(), "bsst count")?;
                    let crc: u32 = parse_field(words.next(), "bsst crc")?;
                    let index = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("bsst index"))?;
                    let filter = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("bsst filter"))?;
                    let roster = words
                        .next()
                        .ok_or(CheckpointSetupError::Corrupt("bsst roster"))?;
                    let handle = if index == "-" {
                        None
                    } else {
                        Some(SstHandle {
                            index: parse_block_id(index)?,
                            filter: parse_block_id(filter)?,
                            roster: parse_block_id(roster)?,
                            versioned: false,
                        })
                    };
                    bssts.push((mindex, 0, count, crc, handle));
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
                    let handle = if index == "-" {
                        None
                    } else {
                        let versioned = match words.next() {
                            None | Some("v1") => false,
                            Some("v2") => true,
                            Some(_) => {
                                return Err(CheckpointSetupError::Corrupt("unknown dsst format"));
                            }
                        };
                        Some(SstHandle {
                            index: parse_block_id(index)?,
                            filter: parse_block_id(filter)?,
                            roster: parse_block_id(roster)?,
                            versioned,
                        })
                    };
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
                Some("view") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    load_legacy_view(storage, line)?;
                }
                tag @ (Some("vw2") | Some("vw3") | Some("vw4")) => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    load_view(
                        storage,
                        line,
                        tag == Some("vw3") || tag == Some("vw4"),
                        tag == Some("vw4"),
                    )?;
                }
                tag @ (Some("mv2") | Some("mv3") | Some("mv4")) => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    load_matview(
                        storage,
                        line,
                        tag == Some("mv3") || tag == Some("mv4"),
                        tag == Some("mv4"),
                    )?;
                }
                tag @ (Some("sq2") | Some("sq3") | Some("sq4")) => {
                    let has_owner = tag == Some("sq3");
                    let has_links = matches!(tag, Some("sq3") | Some("sq4"));
                    let has_generator = tag == Some("sq4");
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
                    let read_hex = |w: Option<&str>, what: &'static str| {
                        w.ok_or(CheckpointSetupError::Corrupt(what))
                            .and_then(decode_hex_name)
                    };
                    let schema = read_hex(words.next(), "sq2 schema missing")?;
                    let name = read_hex(words.next(), "sq2 name missing")?;
                    let data_type: u8 = parse_field(words.next(), "sq2 type")?;
                    let increment: i64 = parse_field(words.next(), "sq2 increment")?;
                    let min_value: i64 = parse_field(words.next(), "sq2 min")?;
                    let max_value: i64 = parse_field(words.next(), "sq2 max")?;
                    let start_value: i64 = parse_field(words.next(), "sq2 start")?;
                    let cache: i64 = parse_field(words.next(), "sq2 cache")?;
                    let cycle: u8 = parse_field(words.next(), "sq2 cycle")?;
                    let last_value: i64 = parse_field(words.next(), "sq2 last")?;
                    let is_called: u8 = parse_field(words.next(), "sq2 is_called")?;
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
                    let owner = if has_links {
                        read_link(&mut words, "sequence owner missing")?
                    } else {
                        None
                    };
                    // sq3 briefly represented ownership and generation as one
                    // link; preserve that manifest's semantics on upgrade.
                    let generator_for = if has_generator {
                        read_link(&mut words, "sequence generator missing")?
                    } else if has_owner {
                        owner
                    } else {
                        None
                    };
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
                    seq.dirty.set(false);
                }
                tag @ (Some("dom") | Some("dom2")) => {
                    let has_parent = tag == Some("dom2");
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
                        };
                    }
                    let spec = crate::storage::DomainSpec {
                        base_domain: (!base_domain.is_empty())
                            .then(|| sql_name(&base_domain))
                            .transpose()?,
                        base_domain_schema: (!base_domain_schema.is_empty())
                            .then(|| sql_name(&base_domain_schema))
                            .transpose()?,
                        base,
                        base_type_mod,
                        not_null: not_null != 0,
                        default_expr: (!default_text.is_empty())
                            .then(|| crate::util::StackStr::from_str(&default_text)),
                        checks,
                        n_checks,
                    };
                    storage
                        .create_domain(sql_name(&schema)?, sql_name(&name)?, spec, 0)
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
                Some("idx") => {
                    finish_pending(storage, &mut slot_of, pending_def.take())?;
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
                    let schema = match words.next() {
                        Some(hex) => decode_hex_name(hex)?,
                        None => "public".to_string(),
                    };
                    let descending_mask: u16 = match words.next() {
                        Some(mask) => parse_field(Some(mask), "idx descending mask")?,
                        None => 0,
                    };
                    let nulls_first_mask: u16 = match words.next() {
                        Some(mask) => parse_field(Some(mask), "idx nulls-first mask")?,
                        None => 0,
                    };
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
                                schema: sql_name(&schema)?,
                                name: sql_name(&name)?,
                                table: sql_name(&table)?,
                                ownership: crate::storage::Ownership::BOOTSTRAP,
                                columns,
                                descending,
                                nulls_first,
                                n_cols,
                                unique: unique != 0,
                                live: true,
                                pending: None,
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

        for (key, mindex, count, bytes, crc) in &ssts {
            let slot =
                slot_of
                    .get(*mindex)
                    .copied()
                    .flatten()
                    .ok_or(CheckpointSetupError::Corrupt(
                        "sst references unknown table",
                    ))?;
            self.rehydrate_sst(storage, key, slot, *count, *bytes, *crc)?;
            // An old whole-object SST loads but is not carried forward: the
            // next checkpoint rewrites the table as a block SST, after which
            // the object is unreferenced and swept.
            if self.prev_ssts.len() <= slot {
                self.prev_ssts.resize(slot + 1, SlotList::EMPTY);
            }
            self.prev_ssts[slot] = SlotList::EMPTY;
            self.referenced.push(crate::stack_format!(64, "{}", key));
        }

        // Block SSTs load in (slot, list index) order so the installed list
        // preserves generation rank for equal legacy keys. Versioned reads
        // choose the greatest admissible commit LSN across every member.
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

        storage
            .rebind_all_stored_query_dependencies()
            .map_err(|error| {
                CheckpointSetupError::ObjectStore(format!(
                    "manifest stored-query dependency rejected: {}",
                    error.message.as_str()
                ))
            })?;
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
        let last_id =
            crate::store::locate_data_block(&mut *blocks, handle, index_buf, block_count - 1)
                .map_err(|_| CheckpointSetupError::Corrupt("sst index unreachable"))?
                .ok_or(CheckpointSetupError::Corrupt("sst index names no blocks"))?;
        let data_len = crate::store::read_data_block(&mut *blocks, &last_id, data_buf, index_buf)
            .map_err(|_| CheckpointSetupError::Corrupt("sst data block unreachable"))?;
        let mut at = 0usize;
        let mut max_rowid: Option<u64> = None;
        while let Some((key, _, _, next)) =
            crate::store::block_keys_at(&data_buf[..data_len], at, handle.versioned)
        {
            max_rowid = Some(key.rowid);
            at = next;
        }
        drop(blocks);
        if let Some(rowid) = max_rowid {
            storage.observe_rowid(rowid);
        }
        Ok(())
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
            .map_err(|e| CheckpointSetupError::ObjectStore(format!("sst footer: {e}")))?;
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
                .map_err(|e| CheckpointSetupError::ObjectStore(format!("sst read: {e}")))?;
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
                let (loc, slice) = storage.heap.append(len).map_err(|e| {
                    CheckpointSetupError::ObjectStore(format!("rehydrate: {}", e.message.as_str()))
                })?;
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
        let pinned_full_list = storage.has_active_snapshots()
            && self.merge_done.is_none()
            && (0..storage.table_count()).any(|slot| {
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
        for slot in 0..storage.table_count().min(MAX_CKPT_TABLES) {
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
        write_manifest(
            &mut self.manifest_buf,
            format_args!("writer {:016x}", self.writer_id),
        )?;

        // Roles are durable catalog authority. Only SCRAM verifier material
        // crosses this object-backed manifest; plaintext passwords never do.
        for (_, role) in storage.live_roles() {
            use core::fmt::Write;
            let attributes = role.attributes;
            let mut name = StackStr::<130>::new();
            for byte in role.name.as_str().as_bytes() {
                let _ = write!(name, "{byte:02x}");
            }
            let mut salt = StackStr::<32>::new();
            let mut stored_key = StackStr::<64>::new();
            let mut server_key = StackStr::<64>::new();
            for byte in attributes.password.salt {
                let _ = write!(salt, "{byte:02x}");
            }
            for byte in attributes.password.stored_key {
                let _ = write!(stored_key, "{byte:02x}");
            }
            for byte in attributes.password.server_key {
                let _ = write!(server_key, "{byte:02x}");
            }
            let mut valid_until = StackStr::<{ 2 * crate::storage::ROLE_VALID_UNTIL_MAX }>::new();
            if attributes.has_valid_until {
                if attributes.valid_until.as_str().is_empty() {
                    let _ = write!(valid_until, "0");
                } else {
                    for byte in attributes.valid_until.as_str().as_bytes() {
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
                | (u16::from(attributes.has_password) << 7)
                | (u16::from(attributes.has_valid_until) << 8);
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
                    attributes.password.iterations,
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

        // Schemas: `nsp <hex-name>` (public is implicit and never written).
        for (_, schema) in storage.live_schemas() {
            if schema.name.as_str() == "public" {
                continue;
            }
            use core::fmt::Write;
            let mut hex = StackStr::<130>::new();
            for b in schema.name.as_str().as_bytes() {
                let _ = write!(hex, "{b:02x}");
            }
            write_manifest(&mut self.manifest_buf, format_args!("nsp {}", hex.as_str()))?;
        }
        // Domains: `dom2 <base-code> <base-typmod> <not-null> <n-checks>
        // <hex-schema> <hex-name> <hex-base-domain> <hex-base-domain-schema> <hex-default>
        // [<hex-cname> <hex-cexpr>]...`. Like enums, domains precede tables
        // because generated domain-array columns bind their runtime slot while
        // the table definition is rebuilt.
        for (_, d) in storage.live_domains() {
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
                "dom2 {} {} {} {} ",
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
                    .map(|name| name.as_str())
                    .unwrap_or(""),
            );
            let _ = write!(line, " ");
            hex(
                &mut line,
                d.base_domain_schema
                    .as_ref()
                    .map(|schema| schema.as_str())
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
        for (_, e) in storage.live_enums() {
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
        for slot in 0..storage.table_count() {
            let table = storage.table(slot);
            if !table.live {
                // A dropped table's recorded list must not linger into the
                // GC keep-set the swap below publishes.
                if slot < self.prev_scratch.len() {
                    self.prev_scratch[slot] = SlotList::EMPTY;
                }
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
            for c in table.def.columns() {
                use core::fmt::Write as _;
                let default_hex = default_to_hex(&c.default_value);
                // Non-constant DEFAULT text, hex-encoded (`0` sentinel = none),
                // placed before the name (which may itself contain spaces).
                let mut dexpr_hex = StackStr::<{ 2 * crate::storage::DEFAULT_EXPR_MAX + 1 }>::new();
                match &c.default_expr {
                    Some(e) => {
                        for b in e.as_str().as_bytes() {
                            let _ = write!(dexpr_hex, "{b:02x}");
                        }
                    }
                    None => {
                        let _ = write!(dexpr_hex, "0");
                    }
                }
                let flags = u8::from(c.not_null)
                    | (u8::from(c.unique) << 1)
                    | (u8::from(c.primary) << 2)
                    | (u8::from(c.auto_increment) << 3)
                    | (u8::from(c.is_generated) << 4)
                    | (u8::from(c.is_identity) << 5)
                    | (u8::from(c.identity_always) << 6);
                // The domain type name, hex-encoded (`0` = ordinary base type),
                // before the name (which may contain spaces).
                let mut domain_schema_hex = StackStr::<130>::new();
                match &c.user_type_schema {
                    Some(schema) => {
                        for byte in schema.as_str().as_bytes() {
                            let _ = write!(domain_schema_hex, "{byte:02x}");
                        }
                    }
                    None => {
                        let _ = write!(domain_schema_hex, "0");
                    }
                }
                let mut domain_hex = StackStr::<130>::new();
                match &c.domain {
                    Some(d) => {
                        for b in d.as_str().as_bytes() {
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
                        "col2 {} {} {} {} {} {} {} {} {}",
                        c.ctype.code(),
                        flags,
                        c.type_mod,
                        default_hex.as_str(),
                        dexpr_hex.as_str(),
                        c.auto_increment_step,
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
            // `ukey <is_primary> <ncols> <c0..cN> <hex-name>`
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
                        "ukey {} {} {}{}",
                        u8::from(uk.is_primary),
                        uk.n_cols,
                        columns.as_str(),
                        hex_name.as_str()
                    ),
                )?;
            }
            // `chk <hex-name> <hex-predicate>`
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
                    format_args!("chk {} {}", hex_name.as_str(), hexpr.as_str()),
                )?;
            }
            // `fkey <ncols> <c..> <nparent> <p..> <on_delete> <on_update> <hex-name> <hex-parent>`
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
                        "fkey {} {}{} {}{} {} {} {} {}",
                        fk.n_cols,
                        columns.as_str(),
                        fk.n_parent_cols,
                        pcols.as_str(),
                        fk.on_delete.code(),
                        fk.on_update.code(),
                        hex_name.as_str(),
                        hparent.as_str(),
                        hparent_schema.as_str()
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
                        if h.versioned { "v2" } else { "v1" },
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
        // Views: `vw2 <hex-SELECT> <hex-schema> <hex-creation-path> <hex-name>`
        // (all hex, so every field survives the space-separated format; the
        // loader still reads the older `view` line for old manifests).
        for (view_slot, view) in storage.views_with_slots() {
            use core::fmt::Write;
            let mut hex = StackStr::<{ 2 * crate::storage::VIEW_SQL_MAX }>::new();
            for b in view.sql.as_str().as_bytes() {
                let _ = write!(hex, "{b:02x}");
            }
            let mut hschema = StackStr::<130>::new();
            for b in view.schema.as_str().as_bytes() {
                let _ = write!(hschema, "{b:02x}");
            }
            let mut hpath = StackStr::<260>::new();
            for b in view.creation_path.as_str().as_bytes() {
                let _ = write!(hpath, "{b:02x}");
            }
            let mut hname = StackStr::<130>::new();
            for b in view.name.as_str().as_bytes() {
                let _ = write!(hname, "{b:02x}");
            }
            write_manifest(
                &mut self.manifest_buf,
                format_args!(
                    "vw4 {} {} {} {} {}",
                    hex.as_str(),
                    hschema.as_str(),
                    hpath.as_str(),
                    hname.as_str(),
                    ManifestDependencies(storage.view_dependencies(view_slot))
                ),
            )?;
        }
        // Materialized views: like `vw2`, plus a trailing populated flag (0/1).
        // The backing table's rows serialize through the ordinary table/dsst
        // loop; this line records only the defining query.
        for (matview_slot, mv) in storage.matviews_with_slots() {
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
                    "mv4 {} {} {} {} {} {}",
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
        // value state (`last_value`, `is_called`). A sequence stores no rows, so
        // this line is its whole durable form.
        for seq in storage.live_sequences() {
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
                    "sq4 {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
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
                    owner_schema.as_str(),
                    owner_table.as_str(),
                    owner_column.as_str(),
                    generator_schema.as_str(),
                    generator_table.as_str(),
                    generator_column.as_str(),
                ),
            )?;
        }
        // Object comments: `cmt <class> <subid> <hex-schema> <hex-name>
        // <hex-text>`. Only committed comments carrying text are written.
        for comment in storage.live_comments() {
            use core::fmt::Write;
            let Some(text) = comment.live else { continue };
            let mut hschema = StackStr::<130>::new();
            for b in comment.schema.as_str().as_bytes() {
                let _ = write!(hschema, "{b:02x}");
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
        // Indexes: `idx <unique> <ncols> <c0..cN> <hex-name> <hex-table>
        // <hex-schema> <descending-mask> <nulls-first-mask>`.
        for index in storage.live_indexes() {
            use core::fmt::Write;
            let mut columns = StackStr::<128>::new();
            for c in &index.columns[..index.n_cols] {
                let _ = write!(columns, "{c} ");
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
            write_manifest(
                &mut self.manifest_buf,
                format_args!(
                    "idx {} {} {}{} {} {} {} {}",
                    u8::from(index.unique),
                    index.n_cols,
                    columns.as_str(),
                    hex_name.as_str(),
                    htable.as_str(),
                    hschema.as_str(),
                    descending_mask,
                    nulls_first_mask,
                ),
            )?;
        }
        // Ownership and ACL authority follows every object definition so a
        // cold manifest load can resolve stable runtime slots from names.
        let mut write_owner = |object: crate::storage::AccessObject| -> Result<(), SqlError> {
            use core::fmt::Write;
            let (schema, name) = storage.access_object_name(object);
            let owner = storage.role(storage.object_owner(object, 0)).name;
            let mut schema_hex = StackStr::<130>::new();
            let mut name_hex = StackStr::<130>::new();
            let mut owner_hex = StackStr::<130>::new();
            for byte in schema.as_str().as_bytes() {
                let _ = write!(schema_hex, "{byte:02x}");
            }
            for byte in name.as_str().as_bytes() {
                let _ = write!(name_hex, "{byte:02x}");
            }
            for byte in owner.as_str().as_bytes() {
                let _ = write!(owner_hex, "{byte:02x}");
            }
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
        };
        for slot in 0..storage.table_count() {
            if storage.table(slot).live {
                write_owner(crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Table,
                    slot: slot as u16,
                })?;
            }
        }
        for (slot, _) in storage.views_with_slots() {
            write_owner(crate::storage::AccessObject {
                class: crate::storage::AccessClass::View,
                slot: slot as u16,
            })?;
        }
        for (slot, _) in storage.matviews_with_slots() {
            write_owner(crate::storage::AccessObject {
                class: crate::storage::AccessClass::MaterializedView,
                slot: slot as u16,
            })?;
        }
        for (slot, _) in storage.sequences_with_slots() {
            write_owner(crate::storage::AccessObject {
                class: crate::storage::AccessClass::Sequence,
                slot: slot as u16,
            })?;
        }
        for (slot, _) in storage.live_schemas() {
            write_owner(crate::storage::AccessObject {
                class: crate::storage::AccessClass::Schema,
                slot: slot as u16,
            })?;
        }
        for (slot, _) in storage.live_domains() {
            write_owner(crate::storage::AccessObject {
                class: crate::storage::AccessClass::Domain,
                slot: slot as u16,
            })?;
        }
        for (slot, _) in storage.live_enums() {
            write_owner(crate::storage::AccessObject {
                class: crate::storage::AccessClass::Enum,
                slot: slot as u16,
            })?;
        }
        for (slot, _) in storage.live_indexes_with_slots() {
            write_owner(crate::storage::AccessObject {
                class: crate::storage::AccessClass::Index,
                slot: slot as u16,
            })?;
        }
        for (_, acl) in storage.live_acls() {
            if !storage.access_object_is_live(acl.object) {
                continue;
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
        for (_, acl) in storage.live_default_acls() {
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
            Some(etag) => Precondition::IfMatch(etag.as_str()),
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
                        Precondition::IfMatch(refreshed.etag.as_str()),
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

        // GC: delete any SST under sst/ not referenced by the new manifest,
        // then any block not on a live SST's roster. Advisory: a failure
        // leaves orphans for the next publish's sweep (mark-and-sweep is
        // idempotent), never a failed checkpoint — the checkpoint's promise
        // was kept at the CAS.
        if let Err(e) = self.collect_garbage() {
            eprintln!(
                "pos3ql: post-checkpoint garbage sweep failed ({}): {}",
                e.sqlstate,
                e.message.as_str()
            );
        }
        if let Err(e) = self.collect_block_garbage(storage) {
            eprintln!(
                "pos3ql: post-checkpoint block sweep failed ({}): {}",
                e.sqlstate,
                e.message.as_str()
            );
        }
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
    /// interrupted write, and is deleted. Overflow defers to the next sweep
    /// rather than deleting anything live.
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
                    eprintln!("pos3ql: block keep-set full; skipping block GC this checkpoint");
                    return Ok(());
                }
                self.roster_scratch.push(*id);
            }
        }
        for prev in self.prev_ssts.iter().flat_map(SlotList::iter) {
            let h = prev.handle;
            if self.roster_scratch.len() + 1 > MAX_KEEP_BLOCKS {
                eprintln!("pos3ql: block keep-set full; skipping block GC this checkpoint");
                return Ok(());
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
                    eprintln!("pos3ql: block keep-set full; skipping block GC this checkpoint");
                    return Ok(());
                }
                let mut id = [0u8; 32];
                id.copy_from_slice(id_bytes);
                self.roster_scratch.push(BlockId(id));
            }
        }
        for slot in 0..storage.table_count() {
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
                    eprintln!("pos3ql: block keep-set full; skipping block GC this checkpoint");
                    return Ok(());
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
            eprintln!("pos3ql: block garbage exceeds one sweep; continuing next checkpoint");
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
            eprintln!("pos3ql: sst garbage exceeds one sweep; continuing next checkpoint");
        }
        Ok(())
    }
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

fn sst_to_sql(e: crate::store::SstError) -> SqlError {
    sql_err!(SQLSTATE_IO, "checkpoint sst: {:?}", e)
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
        let slot = storage.create_table(definition).map_err(|error| {
            CheckpointSetupError::ObjectStore(format!(
                "manifest table rejected: {}",
                error.message.as_str()
            ))
        })?;
        storage.table_mut(slot).serial_last = serials;
        if slot_of.len() <= manifest_index {
            slot_of.resize(manifest_index + 1, None);
        }
        slot_of[manifest_index] = Some(slot);
    }
    Ok(())
}

#[inline(never)]
fn load_legacy_view(storage: &mut Storage, line: &str) -> Result<(), CheckpointSetupError> {
    let mut words = line.split(' ');
    let _tag = words.next();
    let hex = words
        .next()
        .ok_or(CheckpointSetupError::Corrupt("view sql missing"))?;
    if !hex.len().is_multiple_of(2) || hex.len() / 2 > crate::storage::VIEW_SQL_MAX {
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
    let mut buffer = StackStr::<{ crate::storage::VIEW_SQL_MAX }>::new();
    use core::fmt::Write;
    let _ = write!(buffer, "{sql}");
    let mut path = StackStr::<128>::new();
    let _ = write!(path, "\"$user\", public");
    let (new_slot, old_slot) = storage
        .create_view(
            sql_name("public")?,
            sql_name(name)?,
            crate::storage::StoredQueryDefinition {
                sql: buffer,
                creation_path: path,
                dependencies: crate::storage::StoredQueryDependencies::EMPTY,
            },
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
fn load_view(
    storage: &mut Storage,
    line: &str,
    has_dependencies: bool,
    has_referenced_names: bool,
) -> Result<(), CheckpointSetupError> {
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
    let dependencies = if has_dependencies {
        parse_stored_query_dependencies(&mut words, has_referenced_names)?
    } else {
        crate::storage::StoredQueryDependencies::EMPTY
    };
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
fn load_matview(
    storage: &mut Storage,
    line: &str,
    has_dependencies: bool,
    has_referenced_names: bool,
) -> Result<(), CheckpointSetupError> {
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
    let dependencies = if has_dependencies {
        parse_stored_query_dependencies(&mut words, has_referenced_names)?
    } else {
        crate::storage::StoredQueryDependencies::EMPTY
    };
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

impl core::fmt::Display for ManifestDependencies<'_> {
    fn fmt(&self, output: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(output, "{}", self.0.entries().len())?;
        for dependency in self.0.entries() {
            write!(output, " {} ", dependency.class as u8)?;
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
        }
        Ok(())
    }
}

fn parse_stored_query_dependencies(
    words: &mut core::str::Split<'_, char>,
    has_referenced_names: bool,
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
        let schema = decode_hex_name(words.next().ok_or(CheckpointSetupError::Corrupt(
            "stored-query dependency schema missing",
        ))?)?;
        let name = decode_hex_name(words.next().ok_or(CheckpointSetupError::Corrupt(
            "stored-query dependency name missing",
        ))?)?;
        let (referenced_schema, referenced_name) = if has_referenced_names {
            (
                decode_hex_name(words.next().ok_or(CheckpointSetupError::Corrupt(
                    "stored-query referenced schema missing",
                ))?)?,
                decode_hex_name(words.next().ok_or(CheckpointSetupError::Corrupt(
                    "stored-query referenced name missing",
                ))?)?,
            )
        } else {
            (schema.clone(), name.clone())
        };
        dependencies
            .serialized_push(
                class,
                sql_name(&schema)?,
                sql_name(&name)?,
                sql_name(&referenced_schema)?,
                sql_name(&referenced_name)?,
            )
            .map_err(|_| CheckpointSetupError::Corrupt("too many stored-query dependencies"))?;
    }
    Ok(dependencies)
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
        default_expr: None,
        is_generated: false,
        is_identity: false,
        identity_always: false,
        auto_increment_step: 1,
        domain: None,
        user_type_schema: None,
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
        bytes[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| corrupt())?;
    }
    let mut at = 0usize;
    let d = crate::wal::decode_default(&bytes[..n], &mut at).ok_or_else(corrupt)?;
    if at != n {
        return Err(corrupt());
    }
    Ok(d)
}

#[cfg(test)]
mod stored_dependency_tests {
    use super::*;
    use crate::storage::{DependencyClass, StoredQueryDependencies};

    #[test]
    fn manifest_round_trip_preserves_reference_names() {
        let mut dependencies = StoredQueryDependencies::EMPTY;
        dependencies
            .serialized_push(
                DependencyClass::Table,
                SqlName::parse("moved").unwrap(),
                SqlName::parse("current_name").unwrap(),
                SqlName::parse("").unwrap(),
                SqlName::parse("original_name").unwrap(),
            )
            .unwrap();
        let encoded = format!("{}", ManifestDependencies(&dependencies));
        let mut words = encoded.split(' ');
        assert_eq!(
            parse_stored_query_dependencies(&mut words, true).unwrap(),
            dependencies
        );
    }
}
