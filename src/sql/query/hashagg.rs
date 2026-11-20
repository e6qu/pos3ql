//! A hashed grouping run as PostgreSQL's `HashAggregate` runs it
//! (`nodeAgg.c`), reading its input as a stream.
//!
//! The groups live in a `simplehash` table sized as PostgreSQL's `hash_mem`
//! plans it. Once the table reaches its memory or group limit
//! (`hash_agg_check_limits`) it enters spill mode: a row of an existing
//! group stays, any other is written to one of a power of two of partitions
//! chosen by the hash bits `used_bits` onward (`hashagg_spill_tuple`). The
//! partitions become batches on a stack; each is read back into the emptied
//! table, which keeps its bucket count (`ResetTupleHashTable`), and spills
//! again by the next hash bits when it does not fit. Groups leave in the
//! table's iteration order, then each batch's, the last one pushed first,
//! which is the order PostgreSQL returns them.
//! <https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/nodeAgg.c>
//!
//! Partitions are tapes of a [`TapeSet`], a port of `logtape.c`'s block
//! allocation: blocks are preallocated to each tape in runs, freed as a
//! batch reads them and reused lowest first. One set follows PostgreSQL's
//! byte counts, which give `EXPLAIN ANALYZE`'s `Disk Usage`; a second keeps
//! the spilled rows themselves in 8 kB blocks leased from `plan_build_bytes`,
//! so a batch reads only its own blocks and the space a read batch held is
//! reused. A spill that outgrows the pool fails with a named error.
//! <https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/sort/logtape.c>

use std::sync::Arc;

use super::index_paths::aggregate::{hash_agg_set_limits, hash_choose_num_partitions};
use crate::mem::lease_pool::{LeaseError, LeasePool, MutableSlice};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::query::postgres_memory::{HashAggMemory, HashAggSizing};
use crate::sql::query::simplehash::SimpleHash;
use crate::sql_err;

/// `sizeof(TupleHashEntryData)`, which `hash_agg_update_metrics` adds to a
/// group's share of the transition memory.
const TUPLE_HASH_ENTRY: f64 = 16.0;
/// `HASHAGG_HLL_BIT_WIDTH`'s registers.
const HLL_REGISTERS: usize = 32;
/// `BLCKSZ`: a tape block, and the buffer `HASHAGG_READ_BUFFER_SIZE` and
/// `HASHAGG_WRITE_BUFFER_SIZE` charge per open tape.
const BLOCK: usize = 8192;
/// `TapeBlockPayloadSize`: a block less its trailer.
const PAYLOAD: usize = BLOCK - 16;
/// `TAPE_WRITE_PREALLOC_MIN` and `TAPE_WRITE_PREALLOC_MAX`.
const PREALLOC_MIN: usize = 8;
const PREALLOC_MAX: usize = 128;
/// No block.
const NO_BLOCK: u32 = u32::MAX;
/// Bytes a spilled row carries before its values: its hash, its length
/// here and the `t_len` PostgreSQL writes for it.
const SPILL_HEADER: usize = 12;

/// The hash aggregate's tables and tapes did not fit `plan_build_bytes`.
fn pool_full(error: LeaseError) -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "a hash aggregate exceeds plan_build_bytes: {}",
        error
    )
}

/// How a hash aggregate's groups are keyed.
/// Fails once the meta context's bucket array outgrows the chunk limit of
/// a context PostgreSQL's freelist may hand it: after a hash aggregate built
/// under `work_mem` below 1MB earlier in the session, aset.c reuses that
/// aggregate's work context, smaller chunk limit and all, and pos3ql does
/// not track which context the freelist returns.
fn check_context_reuse(memory: &HashAggMemory) -> Result<(), SqlError> {
    if memory.reuse_unmodeled() {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "the memory of a hash aggregate after an earlier hash aggregate of this session ran with work_mem below 1MB is not supported: PostgreSQL reuses that aggregate's memory context, which pos3ql does not model; start a new session, or run every hash aggregate of the session with work_mem of at least 1MB"
        ));
    }
    Ok(())
}

pub(super) trait GroupKeys {
    /// The hash `TupleHashTableHash` gives the row's keys.
    fn hash(&mut self, row: &[u8]) -> Result<u32, SqlError>;
    /// Whether the keys of a held group's row and `row` are equal.
    fn same(&mut self, held: &[u8], row: &[u8]) -> Result<bool, SqlError>;
    /// The `t_len` of the representative tuple a new group of `row` keeps.
    fn entry_len(&mut self, row: &[u8]) -> Result<usize, SqlError>;
    /// The `t_len` of the minimal tuple `hashagg_spill_tuple` writes for
    /// `row`.
    fn spill_len(&mut self, row: &[u8]) -> Result<usize, SqlError>;
}

/// One tape being written.
#[derive(Clone, Copy)]
struct WriteTape {
    first: u32,
    current: u32,
    /// The bytes in the current block.
    pos: usize,
    /// The bytes written in all.
    bytes: u64,
    /// The preallocated blocks: their count, how many were taken, and the
    /// list's size (`prealloc_size`).
    prealloc_len: usize,
    prealloc_taken: usize,
    prealloc_size: usize,
}

impl WriteTape {
    const EMPTY: Self = Self {
        first: NO_BLOCK,
        current: NO_BLOCK,
        pos: 0,
        bytes: 0,
        prealloc_len: 0,
        prealloc_taken: 0,
        prealloc_size: 0,
    };
}

/// A written tape read back from its first block (a batch's input).
#[derive(Clone, Copy)]
struct ReadTape {
    /// The next block to load, and the bytes not yet loaded.
    next: u32,
    unloaded: u64,
    /// The loaded block being read, and the read position in it.
    block: u32,
    at: usize,
    /// The bytes loaded but not read.
    available: usize,
}

/// `LogicalTapeSet` with preallocation (`LogicalTapeSetCreate(true, ...)`),
/// keeping the bytes written when it holds data.
struct TapeSet {
    /// The free blocks, a min-heap (`freeBlocks`).
    free: MutableSlice<u32>,
    /// Each block's successor on its tape.
    next: MutableSlice<u32>,
    /// `nBlocksAllocated` and `nBlocksWritten`.
    allocated: u32,
    written: u32,
    /// The tapes of the spill being written, and their preallocated blocks,
    /// `PREALLOC_MAX` slots each.
    tapes: MutableSlice<WriteTape>,
    prealloc: MutableSlice<u32>,
    /// The blocks' bytes, when the set holds the spilled rows.
    blocks: Option<MutableSlice<u8>>,
}

impl TapeSet {
    fn new(pool: &Arc<LeasePool>, data: bool) -> Result<Self, SqlError> {
        Ok(Self {
            free: pool.lease(0).map_err(pool_full)?,
            next: pool.lease(0).map_err(pool_full)?,
            allocated: 0,
            written: 0,
            tapes: pool.lease(0).map_err(pool_full)?,
            prealloc: pool.lease(0).map_err(pool_full)?,
            blocks: if data {
                Some(pool.lease(0).map_err(pool_full)?)
            } else {
                None
            },
        })
    }

    /// `LogicalTapeCreate` for each of a spill's partitions.
    fn begin(&mut self, tapes: usize) -> Result<(), SqlError> {
        self.tapes.truncate(0);
        for _ in 0..tapes {
            self.tapes.push(WriteTape::EMPTY).map_err(pool_full)?;
        }
        let slots = tapes * PREALLOC_MAX;
        while self.prealloc.len() < slots {
            self.prealloc.push(NO_BLOCK).map_err(pool_full)?;
        }
        Ok(())
    }

    /// `ltsGetFreeBlock`: the lowest free block, or a new one.
    fn free_block(&mut self) -> Result<u32, SqlError> {
        let heap = self.free.as_mut_slice();
        if heap.is_empty() {
            let block = self.allocated;
            self.allocated = block.checked_add(1).ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "a hash aggregate's spill exceeds the tape block numbers"
                )
            })?;
            self.next.push(NO_BLOCK).map_err(pool_full)?;
            if let Some(blocks) = &mut self.blocks {
                blocks.extend_from_slice(&[0u8; BLOCK]).map_err(pool_full)?;
            }
            return Ok(block);
        }
        let lowest = heap[0];
        let last = heap.len() - 1;
        let hole_value = heap[last];
        let size = last;
        let mut hole = 0usize;
        loop {
            let (left, right) = (2 * hole + 1, 2 * hole + 2);
            let child = if left < size && right < size {
                if heap[left] < heap[right] {
                    left
                } else {
                    right
                }
            } else if left < size {
                left
            } else if right < size {
                right
            } else {
                break;
            };
            if heap[child] >= hole_value {
                break;
            }
            heap[hole] = heap[child];
            hole = child;
        }
        if size > 0 {
            heap[hole] = hole_value;
        }
        self.free.truncate(last);
        Ok(lowest)
    }

    /// `ltsReleaseBlock`.
    fn release(&mut self, block: u32) -> Result<(), SqlError> {
        self.free.push(block).map_err(pool_full)?;
        let heap = self.free.as_mut_slice();
        let mut hole = heap.len() - 1;
        while hole != 0 {
            let parent = (hole - 1) / 2;
            if heap[parent] < block {
                break;
            }
            heap[hole] = heap[parent];
            hole = parent;
        }
        heap[hole] = block;
        Ok(())
    }

    /// `ltsGetPreallocBlock`: the tape's lowest preallocated block, after
    /// refilling its list from the free blocks when it ran out.
    fn tape_block(&mut self, tape: usize) -> Result<u32, SqlError> {
        let mut state = self.tapes.as_slice()[tape];
        if state.prealloc_taken == state.prealloc_len {
            state.prealloc_size = if state.prealloc_size == 0 {
                PREALLOC_MIN
            } else {
                (state.prealloc_size * 2).min(PREALLOC_MAX)
            };
            for slot in 0..state.prealloc_size {
                let block = self.free_block()?;
                self.prealloc.as_mut_slice()[tape * PREALLOC_MAX + slot] = block;
            }
            state.prealloc_len = state.prealloc_size;
            state.prealloc_taken = 0;
        }
        let block = self.prealloc.as_slice()[tape * PREALLOC_MAX + state.prealloc_taken];
        state.prealloc_taken += 1;
        self.tapes.as_mut_slice()[tape] = state;
        Ok(block)
    }

    /// `ltsWriteBlock`: the file grows to hold the block.
    fn write_block(&mut self, block: u32) {
        self.written = self.written.max(block + 1);
    }

    /// `LogicalTapeWrite` of `length` bytes, `data` when the set holds them.
    fn write(&mut self, tape: usize, length: usize, data: Option<&[u8]>) -> Result<(), SqlError> {
        if self.tapes.as_slice()[tape].current == NO_BLOCK {
            let block = self.tape_block(tape)?;
            let state = &mut self.tapes.as_mut_slice()[tape];
            state.current = block;
            state.first = block;
        }
        let mut done = 0usize;
        while done < length {
            let state = self.tapes.as_slice()[tape];
            if state.pos >= PAYLOAD {
                let next = self.tape_block(tape)?;
                self.next.as_mut_slice()[state.current as usize] = next;
                self.write_block(state.current);
                let state = &mut self.tapes.as_mut_slice()[tape];
                state.current = next;
                state.pos = 0;
                continue;
            }
            let take = (PAYLOAD - state.pos).min(length - done);
            if let (Some(blocks), Some(data)) = (&mut self.blocks, data) {
                let at = state.current as usize * BLOCK + state.pos;
                blocks.as_mut_slice()[at..at + take].copy_from_slice(&data[done..done + take]);
            }
            let state = &mut self.tapes.as_mut_slice()[tape];
            state.pos += take;
            state.bytes += take as u64;
            done += take;
        }
        Ok(())
    }

    /// `LogicalTapeRewindForRead`: the last block is written and the
    /// unused preallocated blocks are freed.
    fn rewind(&mut self, tape: usize) -> Result<ReadTape, SqlError> {
        let state = self.tapes.as_slice()[tape];
        if state.current != NO_BLOCK {
            self.write_block(state.current);
            self.next.as_mut_slice()[state.current as usize] = NO_BLOCK;
        }
        for slot in state.prealloc_taken..state.prealloc_len {
            let block = self.prealloc.as_slice()[tape * PREALLOC_MAX + slot];
            self.release(block)?;
        }
        Ok(ReadTape {
            next: state.first,
            unloaded: state.bytes,
            block: NO_BLOCK,
            at: 0,
            available: 0,
        })
    }

    /// `LogicalTapeRead` of `length` bytes into `out` when the set holds
    /// them: a block is loaded, and freed, once its first byte is needed.
    fn read(
        &mut self,
        tape: &mut ReadTape,
        length: usize,
        mut out: Option<&mut [u8]>,
    ) -> Result<(), SqlError> {
        let mut done = 0usize;
        while done < length {
            if tape.available == 0 {
                if tape.next == NO_BLOCK {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "unexpected end of a hash aggregate's spill tape"
                    ));
                }
                let block = tape.next;
                self.release(block)?;
                tape.block = block;
                tape.at = 0;
                tape.next = self.next.as_slice()[block as usize];
                let loaded = (PAYLOAD as u64).min(tape.unloaded) as usize;
                tape.unloaded -= loaded as u64;
                tape.available = loaded;
            }
            let take = tape.available.min(length - done);
            if let (Some(blocks), Some(out)) = (&self.blocks, out.as_deref_mut()) {
                let at = tape.block as usize * BLOCK + tape.at;
                out[done..done + take].copy_from_slice(&blocks.as_slice()[at..at + take]);
            }
            tape.at += take;
            tape.available -= take;
            done += take;
        }
        Ok(())
    }
}

/// PostgreSQL's `hyperLogLogState` of 5 bit registers, which estimates the
/// groups a spilled partition holds.
#[derive(Clone, Copy)]
struct HyperLogLog([u8; HLL_REGISTERS]);

impl HyperLogLog {
    const EMPTY: Self = Self([0; HLL_REGISTERS]);

    /// `addHyperLogLog`.
    fn add(&mut self, hash: u32) {
        let index = (hash >> 27) as usize;
        let rest = hash << 5;
        let rank = if rest == 0 {
            28
        } else {
            (rest.leading_zeros() + 1).min(28) as u8
        };
        self.0[index] = self.0[index].max(rank);
    }

    /// `estimateHyperLogLog`.
    fn estimate(&self) -> f64 {
        const TWO_32: f64 = 4_294_967_296.0;
        let registers = HLL_REGISTERS as f64;
        let alpha_mm = 0.697 * registers * registers;
        let sum: f64 = self
            .0
            .iter()
            .map(|&rank| 1.0 / 2f64.powi(i32::from(rank)))
            .sum();
        let mut result = alpha_mm / sum;
        if result <= 2.5 * registers {
            let zeros = self.0.iter().filter(|&&rank| rank == 0).count();
            if zeros != 0 {
                result = registers * (registers / zeros as f64).ln();
            }
        } else if result > TWO_32 / 30.0 {
            result = -TWO_32 * (1.0 - result / TWO_32).ln();
        }
        result
    }
}

/// `HashAggSpill`: the partitions a full table spills new groups to.
#[derive(Clone, Copy)]
struct Spill {
    shift: u32,
    mask: u32,
    partitions: usize,
}

/// `HashAggBatch`: one partition to read back into the table.
#[derive(Clone, Copy)]
struct Batch {
    /// The partition's tape in PostgreSQL's byte counts and in this
    /// aggregate's.
    model: ReadTape,
    data: ReadTape,
    used_bits: u32,
    input_card: f64,
}

/// What `EXPLAIN ANALYZE` reports of a hash aggregate
/// (`show_hashagg_info`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HashAggFigures {
    /// `hash_batches_used`.
    pub(crate) batches: u64,
    /// `hash_mem_peak`, in bytes.
    pub(crate) peak: usize,
    /// `hash_disk_used`, in kB.
    pub(crate) disk_kb: u64,
}

/// The groups of one hashed grouping as PostgreSQL's aggregate forms them.
pub(super) struct HashAgg<K: GroupKeys> {
    keys: K,
    memory: HashAggMemory,
    table: SimpleHash,
    /// Each group's row: its offset and length in `slab`, by identity.
    rows: MutableSlice<(u32, u32)>,
    slab: MutableSlice<u8>,
    /// `hash_spill_mode`: no group is added until the next batch.
    spill_mode: bool,
    /// `hashentrysize`: planned, then measured after each batch.
    entry_size: f64,
    spill: Option<Spill>,
    /// A row went to the spill's partitions.
    spilled_rows: bool,
    /// The spill's per-partition tuple counts and cardinality estimates.
    tuples: MutableSlice<i64>,
    cards: MutableSlice<HyperLogLog>,
    batches: MutableSlice<Batch>,
    /// The hash bits already used and the groups expected, which size the
    /// partitions of the next spill: the plan's, then each batch's.
    used_bits: u32,
    input_groups: f64,
    model: TapeSet,
    data: TapeSet,
    /// A spilled row read back; taken while the row is placed.
    row: Option<MutableSlice<u8>>,
    figures: HashAggFigures,
}

impl<K: GroupKeys> HashAgg<K> {
    /// The aggregate `build_hash_tables` makes for `sizing`, its tables and
    /// tapes leased from `pool`.
    pub(super) fn new(
        keys: K,
        sizing: HashAggSizing,
        pool: &Arc<LeasePool>,
    ) -> Result<Self, SqlError> {
        let memory = HashAggMemory::new(sizing).ok_or_else(|| {
            sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "a hash aggregate grouping by {} columns is not supported",
                sizing.columns
            )
        })?;
        crate::sql::guc::note_hash_aggregate();
        check_context_reuse(&memory)?;
        Ok(Self {
            keys,
            memory,
            table: SimpleHash::new(sizing.buckets, pool)?,
            rows: pool.lease(0).map_err(pool_full)?,
            slab: pool.lease(0).map_err(pool_full)?,
            spill_mode: false,
            entry_size: sizing.entry_size,
            spill: None,
            spilled_rows: false,
            tuples: pool.lease(0).map_err(pool_full)?,
            cards: pool.lease(0).map_err(pool_full)?,
            batches: pool.lease(0).map_err(pool_full)?,
            used_bits: 0,
            input_groups: sizing.groups,
            model: TapeSet::new(pool, false)?,
            data: TapeSet::new(pool, true)?,
            row: Some(pool.lease(0).map_err(pool_full)?),
            figures: HashAggFigures {
                batches: 1,
                peak: 0,
                disk_kb: 0,
            },
        })
    }

    /// Takes one input row: looks its group up (`lookup_hash_entries`).
    pub(super) fn offer(&mut self, row: &[u8]) -> Result<(), SqlError> {
        let hash = self.keys.hash(row)?;
        self.place(hash, row)
    }

    /// Finds the row's group and adds it as a new one unless the table is
    /// in spill mode, in which case a row of no group goes to a partition.
    fn place(&mut self, hash: u32, row: &[u8]) -> Result<(), SqlError> {
        let Self {
            keys,
            table,
            rows,
            slab,
            ..
        } = self;
        let mut same = |held: u32| {
            let (offset, length) = rows.as_slice()[held as usize];
            keys.same(
                &slab.as_slice()[offset as usize..offset as usize + length as usize],
                row,
            )
        };
        if self.spill_mode {
            if !table.contains(hash, &mut same)? {
                self.spill_row(hash, row)?;
            }
            return Ok(());
        }
        let identity = u32::try_from(rows.len()).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "a hash aggregate holds too many groups"
            )
        })?;
        let new = if table.insert(hash, identity, &mut same)? {
            let length = self.keys.entry_len(row)?;
            let offset = self.slab.len();
            self.slab.extend_from_slice(row).map_err(pool_full)?;
            self.rows
                .push((offset as u32, row.len() as u32))
                .map_err(pool_full)?;
            Some(length)
        } else {
            None
        };
        // `SH_INSERT` grew the table before the entry was made, also for a
        // probe run grown too long, which no fill factor announces.
        self.memory.grow_buckets_to(self.table.buckets());
        self.memory.lookup(new);
        check_context_reuse(&self.memory)?;
        if new.is_some() && self.memory.spilled() {
            self.enter_spill_mode()?;
        }
        Ok(())
    }

    /// `hash_agg_enter_spill_mode`, and `hashagg_spill_init` the first
    /// time this pass spills.
    fn enter_spill_mode(&mut self) -> Result<(), SqlError> {
        self.spill_mode = true;
        if self.spill.is_some() {
            return Ok(());
        }
        let (partitions, bits) =
            hash_choose_num_partitions(self.input_groups, self.entry_size, self.used_bits);
        let shift = 32 - self.used_bits - bits;
        let partitions = partitions as usize;
        self.spill = Some(Spill {
            shift,
            mask: if shift < 32 {
                ((partitions as u32) - 1) << shift
            } else {
                0
            },
            partitions,
        });
        self.spilled_rows = false;
        self.tuples.truncate(0);
        self.cards.truncate(0);
        for _ in 0..partitions {
            self.tuples.push(0).map_err(pool_full)?;
            self.cards.push(HyperLogLog::EMPTY).map_err(pool_full)?;
        }
        self.model.begin(partitions)?;
        self.data.begin(partitions)
    }

    /// `hashagg_spill_tuple`: the hash, then the tuple.
    fn spill_row(&mut self, hash: u32, row: &[u8]) -> Result<(), SqlError> {
        let spill = self.spill.expect("spill mode has partitions");
        let partition = if spill.shift < 32 {
            ((hash & spill.mask) >> spill.shift) as usize
        } else {
            0
        };
        self.spilled_rows = true;
        self.tuples.as_mut_slice()[partition] += 1;
        self.cards.as_mut_slice()[partition].add(crate::sql::identity::hash_uint32(hash));
        let spilled = self.keys.spill_len(row)?;
        self.model.write(partition, 4 + spilled, None)?;
        let length = u32::try_from(row.len()).expect("a row fits a block payload");
        let mut header = [0u8; SPILL_HEADER];
        header[..4].copy_from_slice(&hash.to_le_bytes());
        header[4..8].copy_from_slice(&length.to_le_bytes());
        header[8..].copy_from_slice(&(spilled as u32).to_le_bytes());
        self.data.write(partition, SPILL_HEADER, Some(&header))?;
        self.data.write(partition, row.len(), Some(row))
    }

    /// `hashagg_spill_finish`: each partition that holds rows becomes a
    /// batch, pushed in partition order. Returns the spill's partitions:
    /// the input's once it entered spill mode, a batch's once it spilled a
    /// row (`agg_refill_hash_table` creates the partitions then).
    fn finish_spill(&mut self, from_tape: bool) -> Result<usize, SqlError> {
        let Some(spill) = self.spill.take() else {
            return Ok(0);
        };
        if from_tape && !self.spilled_rows {
            return Ok(0);
        }
        for partition in 0..spill.partitions {
            if self.tuples.as_slice()[partition] == 0 {
                continue;
            }
            let model = self.model.rewind(partition)?;
            let data = self.data.rewind(partition)?;
            self.batches
                .push(Batch {
                    model,
                    data,
                    used_bits: 32 - spill.shift,
                    input_card: self.cards.as_slice()[partition].estimate(),
                })
                .map_err(pool_full)?;
            self.figures.batches += 1;
        }
        Ok(spill.partitions)
    }

    /// `hash_agg_update_metrics`: the peak memory with the tapes' buffers,
    /// the tapes' file size, and the size of a group the next spill
    /// plans by.
    fn update_metrics(&mut self, from_tape: bool, partitions: usize) {
        let buffers = partitions * BLOCK + if from_tape { BLOCK } else { 0 };
        self.figures.peak = self.figures.peak.max(self.memory.peak() + buffers);
        self.figures.disk_kb = self
            .figures
            .disk_kb
            .max(u64::from(self.model.written) * (BLOCK as u64 / 1024));
        if self.memory.entries() > 0 {
            self.entry_size = TUPLE_HASH_ENTRY
                + self.memory.transition_allocated() as f64 / self.memory.entries() as f64;
        }
    }

    /// The input ended (`hashagg_finish_initial_spills`).
    pub(super) fn finish_input(&mut self) -> Result<(), SqlError> {
        let partitions = self.finish_spill(false)?;
        self.update_metrics(false, partitions);
        self.spill_mode = false;
        Ok(())
    }

    /// `agg_refill_hash_table`: reads the next batch into the emptied
    /// table. False when none is left.
    fn refill(&mut self) -> Result<bool, SqlError> {
        let Some(mut batch) = self.batches.pop() else {
            return Ok(false);
        };
        let (memory_limit, groups_limit, _) =
            hash_agg_set_limits(self.entry_size, batch.input_card, batch.used_bits);
        self.rows.truncate(0);
        self.slab.truncate(0);
        self.table.reset();
        self.memory.start_batch(memory_limit, groups_limit as u64);
        self.spill_mode = false;
        self.used_bits = batch.used_bits;
        self.input_groups = batch.input_card;
        while batch.data.next != NO_BLOCK || batch.data.available > 0 {
            let mut header = [0u8; SPILL_HEADER];
            self.data
                .read(&mut batch.data, SPILL_HEADER, Some(&mut header))?;
            let hash = u32::from_le_bytes(header[..4].try_into().expect("a hash"));
            let length = u32::from_le_bytes(header[4..8].try_into().expect("a length")) as usize;
            let spilled = u32::from_le_bytes(header[8..].try_into().expect("a length")) as usize;
            self.model.read(&mut batch.model, 4 + spilled, None)?;
            let mut row = self.row.take().expect("the read row buffer");
            row.truncate(0);
            while row.len() < length {
                row.push(0).map_err(pool_full)?;
            }
            self.data
                .read(&mut batch.data, length, Some(row.as_mut_slice()))?;
            let placed = self.place(hash, row.as_slice());
            self.row = Some(row);
            placed?;
        }
        let partitions = self.finish_spill(true)?;
        self.update_metrics(true, partitions);
        self.spill_mode = false;
        Ok(true)
    }

    /// Hands every group's row to `visit` in PostgreSQL's order, until it
    /// returns false.
    pub(super) fn emit(
        &mut self,
        visit: &mut dyn FnMut(&[u8]) -> Result<bool, SqlError>,
    ) -> Result<(), SqlError> {
        loop {
            let mut more = true;
            {
                let rows = self.rows.as_slice();
                let slab = self.slab.as_slice();
                self.table.iterate(&mut |identity| {
                    let (offset, length) = rows[identity as usize];
                    more = visit(&slab[offset as usize..offset as usize + length as usize])?;
                    Ok(more)
                })?;
            }
            if !more || !self.refill()? {
                return Ok(());
            }
        }
    }

    /// What `EXPLAIN ANALYZE` reports, once the groups were emitted.
    pub(super) fn figures(&self) -> HashAggFigures {
        self.figures
    }
}

/// The keys of a `SELECT DISTINCT` of projected rows: every output column,
/// hashed in the order `positions` gives.
pub(super) struct ProjectedKeys<'a> {
    pub(super) storage: &'a crate::storage::Storage,
    pub(super) txid: u32,
    pub(super) arena: &'a crate::mem::arena::Arena,
    pub(super) positions: &'a [usize],
    pub(super) collations: &'a [crate::sql::ast::Collation],
    /// Every output column, which a group's tuple holds.
    pub(super) columns: &'a [usize],
}

impl GroupKeys for ProjectedKeys<'_> {
    fn hash(&mut self, row: &[u8]) -> Result<u32, SqlError> {
        for (index, &collation) in self.collations.iter().enumerate() {
            self.storage.validate_text_collation(
                collation,
                &crate::sql::exec::decode_projected_pub(row, index),
            )?;
        }
        Ok(crate::sql::query::simplehash::murmur32(
            super::group::hash_row_keys(self.storage, self.txid, row, self.positions, self.arena)?,
        ))
    }

    fn same(&mut self, held: &[u8], row: &[u8]) -> Result<bool, SqlError> {
        for (index, &collation) in self.collations.iter().enumerate() {
            let ordering = super::materialize::compare_materialized_key(
                self.storage,
                self.txid,
                collation,
                crate::sql::exec::ProjectedValue::column(held, index),
                crate::sql::exec::ProjectedValue::column(row, index),
            )?;
            if !ordering.is_eq() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn entry_len(&mut self, row: &[u8]) -> Result<usize, SqlError> {
        // A composite value's fields are sized through the catalog.
        let sizing = crate::sql::query::postgres_memory::Sizing {
            catalog: Some((self.storage, self.txid)),
            scratch: self.arena,
        };
        crate::sql::query::postgres_memory::projected_tuple_len(
            Some(&sizing),
            row,
            Some(self.columns),
        )
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "a hash aggregate grouping by a value without a PostgreSQL storage size is not supported"
                )
            })
    }

    fn spill_len(&mut self, row: &[u8]) -> Result<usize, SqlError> {
        self.entry_len(row)
    }
}

/// Groups already formed: each input row is its index, in arrival order,
/// keyed by its group.
struct IndexedKeys<'k> {
    group_of: &'k [u32],
    /// Each group's table hash and representative tuple's `t_len`.
    hashes: &'k [u32],
    entry_lens: &'k [usize],
    /// Each row's spilled tuple's `t_len`, when it holds more than its
    /// group's keys.
    spill_lens: Option<&'k [usize]>,
}

impl IndexedKeys<'_> {
    fn index(row: &[u8]) -> usize {
        u32::from_le_bytes(row.try_into().expect("a row index")) as usize
    }
}

impl GroupKeys for IndexedKeys<'_> {
    fn hash(&mut self, row: &[u8]) -> Result<u32, SqlError> {
        Ok(self.hashes[self.group_of[Self::index(row)] as usize])
    }

    fn same(&mut self, held: &[u8], row: &[u8]) -> Result<bool, SqlError> {
        Ok(self.group_of[Self::index(held)] == self.group_of[Self::index(row)])
    }

    fn entry_len(&mut self, row: &[u8]) -> Result<usize, SqlError> {
        Ok(self.entry_lens[self.group_of[Self::index(row)] as usize])
    }

    fn spill_len(&mut self, row: &[u8]) -> Result<usize, SqlError> {
        let index = Self::index(row);
        Ok(match self.spill_lens {
            Some(lengths) => lengths[index],
            None => self.entry_lens[self.group_of[index] as usize],
        })
    }
}

/// Runs rows whose groups are known through PostgreSQL's hash aggregate:
/// row `r` (in arrival order) belongs to group `group_of[r]`, whose table
/// hash is `hashes[g]` and representative tuple `entry_lens[g]` bytes; the
/// tuple a row spills is `spill_lens[r]` bytes, or its group's. Writes
/// each group, in the order the aggregate returns it, to `order` and
/// returns what `EXPLAIN ANALYZE` reports.
pub(super) fn hashed_group_order(
    pool: &Arc<LeasePool>,
    sizing: HashAggSizing,
    group_of: &[u32],
    hashes: &[u32],
    entry_lens: &[usize],
    spill_lens: Option<&[usize]>,
    order: &mut [u32],
) -> Result<HashAggFigures, SqlError> {
    let mut aggregate = HashAgg::new(
        IndexedKeys {
            group_of,
            hashes,
            entry_lens,
            spill_lens,
        },
        sizing,
        pool,
    )?;
    for row in 0..group_of.len() {
        aggregate.offer(&(row as u32).to_le_bytes())?;
    }
    aggregate.finish_input()?;
    let mut count = 0usize;
    aggregate.emit(&mut |row| {
        order[count] = group_of[IndexedKeys::index(row)];
        count += 1;
        Ok(true)
    })?;
    debug_assert_eq!(count, order.len());
    Ok(aggregate.figures())
}

/// Projected rows already held in the statement arena, each offered by
/// its index so a group keeps only that.
pub(super) struct HeldRows<'r, 'a> {
    pub(super) keys: ProjectedKeys<'a>,
    pub(super) rows: &'r [&'a [u8]],
}

impl GroupKeys for HeldRows<'_, '_> {
    fn hash(&mut self, row: &[u8]) -> Result<u32, SqlError> {
        let row = self.rows[IndexedKeys::index(row)];
        self.keys.hash(row)
    }

    fn same(&mut self, held: &[u8], row: &[u8]) -> Result<bool, SqlError> {
        let (held, row) = (
            self.rows[IndexedKeys::index(held)],
            self.rows[IndexedKeys::index(row)],
        );
        self.keys.same(held, row)
    }

    fn entry_len(&mut self, row: &[u8]) -> Result<usize, SqlError> {
        let row = self.rows[IndexedKeys::index(row)];
        self.keys.entry_len(row)
    }

    fn spill_len(&mut self, row: &[u8]) -> Result<usize, SqlError> {
        self.entry_len(row)
    }
}

/// Rows held by the caller, offered by index: each row's table hash and
/// tuple `t_len` precomputed, `same` comparing two rows' keys by index.
struct IndexedRows<'k, F: FnMut(usize, usize) -> Result<bool, SqlError>> {
    hashes: &'k [u32],
    lens: &'k [usize],
    same: F,
}

impl<F: FnMut(usize, usize) -> Result<bool, SqlError>> GroupKeys for IndexedRows<'_, F> {
    fn hash(&mut self, row: &[u8]) -> Result<u32, SqlError> {
        Ok(self.hashes[IndexedKeys::index(row)])
    }

    fn same(&mut self, held: &[u8], row: &[u8]) -> Result<bool, SqlError> {
        (self.same)(IndexedKeys::index(held), IndexedKeys::index(row))
    }

    fn entry_len(&mut self, row: &[u8]) -> Result<usize, SqlError> {
        Ok(self.lens[IndexedKeys::index(row)])
    }

    fn spill_len(&mut self, row: &[u8]) -> Result<usize, SqlError> {
        self.entry_len(row)
    }
}

/// Runs held rows through PostgreSQL's hash aggregate in arrival order:
/// row `r` has table hash `hashes[r]` and a tuple of `lens[r]` bytes, and
/// `same` compares two rows' keys. Writes the first row of each group, in
/// the order the aggregate returns the groups, to `order`, and returns
/// their count and what `EXPLAIN ANALYZE` reports.
pub(super) fn hashed_row_order(
    pool: &Arc<LeasePool>,
    sizing: HashAggSizing,
    hashes: &[u32],
    lens: &[usize],
    same: impl FnMut(usize, usize) -> Result<bool, SqlError>,
    order: &mut [u32],
) -> Result<(usize, HashAggFigures), SqlError> {
    let mut aggregate = HashAgg::new(IndexedRows { hashes, lens, same }, sizing, pool)?;
    for row in 0..hashes.len() {
        aggregate.offer(&(row as u32).to_le_bytes())?;
    }
    aggregate.finish_input()?;
    let mut count = 0usize;
    aggregate.emit(&mut |row| {
        order[count] = IndexedKeys::index(row) as u32;
        count += 1;
        Ok(true)
    })?;
    Ok((count, aggregate.figures()))
}

#[cfg(all(test, not(feature = "sql-fixture-suite")))]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;
    use crate::mem::lease_pool::Purpose;

    fn pool() -> Arc<LeasePool> {
        let mut budget = Budget::new(64 << 20);
        LeasePool::new(&mut budget, Purpose::PLAN_BUILD, 16 << 20, 64).unwrap()
    }

    #[test]
    fn tapes_preallocate_and_reuse_blocks_as_logtape_does() {
        let pool = pool();
        let mut tapes = TapeSet::new(&pool, true).unwrap();
        tapes.begin(2).unwrap();
        // Each tape's first write preallocates eight blocks: 0-7, then 8-15.
        tapes.write(0, 3, Some(b"abc")).unwrap();
        tapes.write(1, 2, Some(b"de")).unwrap();
        let mut first = tapes.rewind(0).unwrap();
        tapes.rewind(1).unwrap();
        // Writing block 8 fills the file up to it.
        assert_eq!(tapes.written, 9);
        assert_eq!(tapes.allocated, 16);
        let mut out = [0u8; 3];
        tapes.read(&mut first, 3, Some(&mut out)).unwrap();
        assert_eq!(&out, b"abc");
        // The freed blocks come back lowest first.
        tapes.begin(1).unwrap();
        tapes.write(0, PAYLOAD + 1, None).unwrap();
        let state = tapes.tapes.as_slice()[0];
        assert_eq!((state.first, state.current), (0, 1));
    }

    #[test]
    fn hyperloglog_estimates_as_postgresql() {
        let mut registers = HyperLogLog::EMPTY;
        assert_eq!(registers.estimate(), 0.0);
        for value in 0..1000u32 {
            registers.add(crate::sql::identity::hash_uint32(value));
        }
        let estimate = registers.estimate();
        assert!((700.0..1300.0).contains(&estimate), "{estimate}");
    }
}
