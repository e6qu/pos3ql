//! A sorted string table over the block grid.
//!
//! An SST is a table's row versions written once, in key order, and never changed —
//! which is what lets it be a run of immutable blocks rather than a file that
//! is seeked within. Current rows are packed into [`BlockType::SstDataV2`]
//! blocks in key order (row-packed or self-describing PAX), and sparse
//! [`BlockType::SstIndexV2`] leaves record,
//! for each data block, the first key it holds and the block's identity. A
//! large SST adds one root over those leaves. Given the root identity a reader
//! can find any key; nothing else about the SST needs naming.
//!
//! The index is *sparse* — one entry per data block, not per row. Finding a key
//! is a binary search of the index for the last block whose first key does not
//! exceed the target, then a scan of that one block. A lookup reads a filter,
//! one index leaf and one data block (plus the root for a multi-leaf SST).
//! That is the whole point of the sparse index: lookup cost stays bounded as
//! the table grows, and the index is cacheable alongside the data.
//!
//! SSTs key versions by `(rowid, commit_lsn)`: row identities ascend, and
//! versions of one row descend by commit LSN so a snapshot lookup finds the
//! newest admissible image first.

use crate::mem::arena::Arena;
use crate::sql::types::ColType;
use crate::storage::rowenc::{self, MAX_COLUMNS};

use super::bloom::{self, FILTER_BYTES};
use super::{BlockId, BlockStore, BlockType, MAX_PAYLOAD, StoreError};

/// What a finished SST is named by: the index block a reader searches, and the
/// filter block it checks first to skip an SST that cannot hold a key. The
/// filter is `None` only for an SST with no rows, which has neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SstHandle {
    pub(crate) index: BlockId,
    pub(crate) filter: BlockId,
    /// The SST's complete block roster: every identity it comprises (data,
    /// chain, filter, index), so garbage collection can enumerate an SST by
    /// reading one block instead of all of them.
    pub(crate) roster: BlockId,
    /// Data entries name verified extents in immutable packed containers.
    /// False retains the direct content-addressed data-block format.
    pub(crate) packed: bool,
}

/// One physical data-block location. A packed reference names both the
/// container extent used for the range read and the logical block identity
/// used to verify the returned bytes before they enter a cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DataBlockRef {
    Direct(BlockId),
    Packed {
        container: BlockId,
        offset: u32,
        length: u32,
        id: BlockId,
    },
}

impl DataBlockRef {
    fn direct(id: BlockId) -> Self {
        Self::Direct(id)
    }

    pub(crate) fn id(self) -> BlockId {
        match self {
            Self::Direct(id) | Self::Packed { id, .. } => id,
        }
    }
}

/// A durable row-version key. Ordering is rowid ascending, then commit LSN
/// descending: all versions of one row are contiguous and newest-first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SstKey {
    pub(crate) rowid: u64,
    pub(crate) commit_lsn: u64,
}

impl SstKey {
    pub(crate) const MIN: Self = Self {
        rowid: 0,
        commit_lsn: u64::MAX,
    };

    pub(crate) const fn newest(rowid: u64) -> Self {
        Self {
            rowid,
            commit_lsn: u64::MAX,
        }
    }

    pub(crate) const fn at(rowid: u64, commit_lsn: u64) -> Self {
        Self { rowid, commit_lsn }
    }

    fn successor(self) -> Option<Self> {
        if self.commit_lsn > 0 {
            Some(Self::at(self.rowid, self.commit_lsn - 1))
        } else {
            self.rowid.checked_add(1).map(Self::newest)
        }
    }
}

impl Ord for SstKey {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.rowid
            .cmp(&other.rowid)
            .then_with(|| other.commit_lsn.cmp(&self.commit_lsn))
    }
}

impl PartialOrd for SstKey {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// `rowid` u64 | `commit_lsn` u64 | `len` u32. The row bytes follow.
/// `len`'s high bit marks a *chained* entry: a row too large for one block,
/// whose payload continues in overflow blocks. The masked low bits are the
/// row's total length; the entry body is then `n_chunks` u16, the overflow
/// blocks' identities, and the head chunk inline.
const VERSIONED_ENTRY_HEADER: usize = 20;
/// Largest row an execution cursor can read without following an overflow
/// chain. External runs enforce this before handing a row to the writer.
pub(crate) const MAX_INLINE_ROW: usize = MAX_PAYLOAD - VERSIONED_ENTRY_HEADER;

/// High bit of the entry length: the row continues in overflow blocks.
const CHAIN_FLAG: u32 = 1 << 31;

/// Tombstone bit: the entry records a deletion, not a row — a delta SST's
/// way of saying an older SST's version of this key is gone. Carries no
/// payload.
const TOMB_FLAG: u32 = 1 << 30;

/// The largest assembled row a reader's scan scratch admits: the chained
/// head chunk plus every overflow block.
pub(crate) const MAX_ASSEMBLED: usize =
    (MAX_PAYLOAD - VERSIONED_ENTRY_HEADER - 2 - MAX_CHAIN * 32) + MAX_CHAIN * MAX_PAYLOAD;

/// The most overflow blocks one chained row may span. With ~256 KiB blocks
/// this caps a single row at about 4 MiB — far above anything the engine's
/// arenas admit — and exceeding it is a loud error, never truncation.
const MAX_CHAIN: usize = 16;
const PAX_V2_MAGIC: u32 = 0x3258_4150;
const PAX_COLUMN_MAGIC: u32 = 0x3143_4150;
const PAX_ROW_HEADER: usize = 8 + 8 + 4;
const PAX_V2_ROW_HEADER: usize = PAX_ROW_HEADER + 4;
const PACKED_DATA_REF_BYTES: usize = 32 + 4 + 4 + 32;
/// PAX data groups are deliberately smaller than their enclosing container so
/// one ranged object can carry several independently cacheable groups.
const PACKED_PAX_TARGET: usize = MAX_PAYLOAD / 2;

const VERSIONED_INDEX_ENTRY: usize = 16 + 32;
const PACKED_VERSIONED_INDEX_ENTRY: usize = 16 + 32 + 4 + 4 + 32;

/// The most data blocks a single-block index can point at. A larger SST needs a
/// multi-block index, which is a later concern; this bound is checked and raised
/// rather than silently overrun.
const MAX_DATA_BLOCKS: usize = MAX_PAYLOAD / PACKED_VERSIONED_INDEX_ENTRY;

/// The most block identities one roster block can list — and so the most
/// blocks one SST may comprise. Checked and raised, never overrun.
const MAX_ROSTER: usize = MAX_PAYLOAD / 32;

/// Building an SST failed. Distinct from [`StoreError`] because these are the
/// writer's own limits (a row too big for a block, more blocks than the index
/// can hold), not the store's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SstError {
    /// A single row exceeds even the chained-row bound (`MAX_CHAIN` blocks).
    RowTooLarge,
    /// More data blocks than one index block can point at.
    TooManyBlocks,
    /// Rows were not handed to the writer in ascending key order.
    KeyOutOfOrder,
    /// A PAX group could not represent a row supplied by its table writer.
    PaxEncoding,
    /// The block store failed.
    Store(StoreError),
}

impl From<StoreError> for SstError {
    fn from(e: StoreError) -> Self {
        SstError::Store(e)
    }
}

/// Writes rows into data blocks in key order and, at the end, the index block
/// that names them. Rows are buffered until a data block is full, so a block is
/// flushed only once it cannot take the next row — no block is written
/// half-empty except the last.
///
/// The writer owns its whole state — buffers and cursors, no borrow of an
/// arena — so an owner can hold a half-written SST across checkpoint beats
/// (the paced merge does) and reuse one writer for slice after slice.
/// Construction allocates (startup or tests only); `reset` returns a used
/// writer to empty without touching the allocator.
pub(crate) struct SstWriter {
    /// Rows accumulating for the current data block.
    pending: Box<[u8]>,
    pending_len: usize,
    /// The first key in the current data block, set when its first row lands.
    pending_first: Option<SstKey>,
    /// The index as it grows: `(first key, block_id)` per flushed data block.
    index: Box<[(SstKey, DataBlockRef)]>,
    index_len: usize,
    /// The last key written, so out-of-order rows are caught rather than
    /// producing an SST whose binary search silently misses them.
    last_key: Option<SstKey>,
    /// The filter ladder: every key is set into all three candidate sizes,
    /// and finish keeps the smallest whose bits-per-key stays healthy — a
    /// small SST no longer pays a 128 KiB filter for a handful of rows.
    filters: [Box<[u8]>; FILTER_TIERS.len()],
    key_count: usize,
    /// Every block identity written so far (data and chain blocks), for the
    /// roster.
    roster: Box<[BlockId]>,
    roster_len: usize,
    /// LZ4 staging for data-block flushes: the smaller of raw/compressed is
    /// what gets stored.
    compress_buf: Box<[u8]>,
    /// Columnar payload assembled from `pending` before compression.
    pax_buf: Box<[u8]>,
    pax_schema: [ColType; MAX_COLUMNS],
    pax_refs: [DataBlockRef; MAX_COLUMNS],
    pax_columns: usize,
    pax_enabled: bool,
    /// Framed logical PAX blocks waiting to become one immutable container.
    packed: Box<[u8]>,
    packed_len: usize,
    packed_index_start: usize,
    /// Flushed index leaves: `(first key, data_block_count, id)`. One leaf
    /// makes the classic single-block index; more make a two-level one, so
    /// an SST is no longer capped at one index block's worth of data.
    leaves: Box<[(SstKey, u32, BlockId)]>,
    leaves_len: usize,
}

/// The filter ladder's sizes. Ten bits per key is the design point (~1%
/// false positives at seven hashes); the tiers cover ~1.5k, ~13k and ~100k
/// keys, degrading gracefully past the top as before.
const FILTER_TIERS: [usize; 3] = [2 * 1024, 16 * 1024, FILTER_BYTES];

/// The most index leaves a root block tracks: at 44 bytes per entry a root
/// could hold ~5900, but the writer's own memory bounds it lower — still
/// about 26 million data blocks (terabytes) per SST, checked and raised
/// rather than overrun.
const MAX_LEAVES: usize = 4096;

/// A root index block starts with this in place of a leaf's entry count.
/// Unambiguous: a leaf's count is at most `MAX_DATA_BLOCKS` (~6.5k).
pub(crate) const INDEX_ROOT_MAGIC: u32 = 0xFFFF_FFFF;

const VERSIONED_ROOT_ENTRY: usize = 16 + 4 + 32;

impl SstWriter {
    /// Allocates the writer's fixed buffers (about 0.9 MiB). Startup only —
    /// after the freeze, `reset` is how a writer is reused.
    pub(crate) fn new() -> Self {
        Self {
            pending: vec![0u8; MAX_PAYLOAD].into_boxed_slice(),
            pending_len: 0,
            pending_first: None,
            index: vec![(SstKey::MIN, DataBlockRef::Direct(BlockId([0u8; 32]))); MAX_DATA_BLOCKS]
                .into_boxed_slice(),
            index_len: 0,
            last_key: None,
            filters: [
                vec![0u8; FILTER_TIERS[0]].into_boxed_slice(),
                vec![0u8; FILTER_TIERS[1]].into_boxed_slice(),
                vec![0u8; FILTER_TIERS[2]].into_boxed_slice(),
            ],
            key_count: 0,
            roster: vec![BlockId([0u8; 32]); MAX_ROSTER].into_boxed_slice(),
            roster_len: 0,
            compress_buf: vec![0u8; MAX_PAYLOAD].into_boxed_slice(),
            pax_buf: vec![0u8; MAX_PAYLOAD].into_boxed_slice(),
            pax_schema: [ColType::Bool; MAX_COLUMNS],
            pax_refs: [DataBlockRef::Direct(BlockId([0u8; 32])); MAX_COLUMNS],
            pax_columns: 0,
            pax_enabled: false,
            packed: vec![0u8; MAX_PAYLOAD].into_boxed_slice(),
            packed_len: 0,
            packed_index_start: 0,
            leaves: vec![(SstKey::MIN, 0u32, BlockId([0u8; 32])); MAX_LEAVES].into_boxed_slice(),
            leaves_len: 0,
        }
    }

    /// The fixed bytes one writer reserves, for budget estimates.
    pub(crate) fn budget_bytes() -> usize {
        4 * MAX_PAYLOAD // pending + PAX + compression + packed-container staging
            + MAX_DATA_BLOCKS * core::mem::size_of::<(SstKey, DataBlockRef)>()
            + FILTER_TIERS.iter().sum::<usize>()
            + MAX_ROSTER * 32
            + MAX_LEAVES * core::mem::size_of::<(SstKey, u32, BlockId)>()
    }

    /// Empties the writer for its next SST. Allocation-free.
    pub(crate) fn reset(&mut self) {
        self.pending_len = 0;
        self.pending_first = None;
        self.index_len = 0;
        self.last_key = None;
        for filter in &mut self.filters {
            filter.fill(0);
        }
        self.key_count = 0;
        self.roster_len = 0;
        self.leaves_len = 0;
        self.packed_len = 0;
        self.packed_index_start = 0;
        self.pax_schema.fill(ColType::Bool);
        self.pax_refs.fill(DataBlockRef::Direct(BlockId([0u8; 32])));
        self.pax_columns = 0;
        self.pax_enabled = false;
    }

    /// Selects the table row layout for the next SST.  Callers must choose it
    /// before appending; execution runs retain the ordinary row layout.
    pub(crate) fn set_pax_schema(&mut self, schema: &[ColType]) -> Result<(), SstError> {
        if self.pending_len != 0
            || self.index_len != 0
            || self.leaves_len != 0
            || schema.len() > MAX_COLUMNS
        {
            return Err(SstError::PaxEncoding);
        }
        self.pax_schema[..schema.len()].copy_from_slice(schema);
        self.pax_columns = schema.len();
        self.pax_enabled = true;
        Ok(())
    }

    /// The identities written so far — a garbage sweep running while a
    /// half-built SST is in flight must keep these alive.
    pub(crate) fn roster_so_far(&self) -> &[BlockId] {
        &self.roster[..self.roster_len]
    }

    /// Appends one row. Flushes the current data block first when the row would
    /// not fit, so every block but the last is filled as far as the next row
    /// allows.
    pub(crate) fn append(
        &mut self,
        store: &mut dyn BlockStore,
        rowid: u64,
        row: &[u8],
    ) -> Result<(), SstError> {
        self.append_version(store, SstKey::at(rowid, 0), row)
    }

    /// Appends one committed image under its durable LSN.
    pub(crate) fn append_version(
        &mut self,
        store: &mut dyn BlockStore,
        key: SstKey,
        row: &[u8],
    ) -> Result<(), SstError> {
        if let Some(last) = self.last_key
            && key <= last
        {
            return Err(SstError::KeyOutOfOrder);
        }
        let entry = VERSIONED_ENTRY_HEADER + row.len();
        if self.pax_enabled && entry > MAX_PAYLOAD {
            return Err(SstError::PaxEncoding);
        }
        if entry > MAX_PAYLOAD {
            return self.append_chained(store, key, row);
        }
        let limit = if self.pax_enabled {
            PACKED_PAX_TARGET.saturating_sub(128)
        } else {
            MAX_PAYLOAD
        };
        if self.pending_len != 0 && self.pending_len + entry > limit {
            self.flush_data(store)?;
        }
        let at = self.pending_len;
        self.pending[at..at + 8].copy_from_slice(&key.rowid.to_le_bytes());
        self.pending[at + 8..at + 16].copy_from_slice(&key.commit_lsn.to_le_bytes());
        self.pending[at + 16..at + 20].copy_from_slice(&(row.len() as u32).to_le_bytes());
        self.pending[at + VERSIONED_ENTRY_HEADER..at + entry].copy_from_slice(row);
        self.pending_len += entry;
        if self.pending_first.is_none() {
            self.pending_first = Some(key);
        }
        self.last_key = Some(key);
        for filter in &mut self.filters {
            bloom::insert(filter, key.rowid);
        }
        self.key_count += 1;
        Ok(())
    }

    /// A row too large for one block: its tail is written as overflow blocks
    /// first (so their identities are known), then a head entry — alone in its
    /// own data block — carries the chain's identities and the leading chunk.
    fn append_chained(
        &mut self,
        store: &mut dyn BlockStore,
        key: SstKey,
        row: &[u8],
    ) -> Result<(), SstError> {
        // The head block holds the entry header, the chunk count, up to
        // MAX_CHAIN identities, and the head chunk; overflow blocks are raw.
        let head_room = MAX_PAYLOAD - VERSIONED_ENTRY_HEADER - 2 - MAX_CHAIN * 32;
        let tail = &row[head_room..];
        let n_chunks = tail.len().div_ceil(MAX_PAYLOAD);
        if n_chunks > MAX_CHAIN {
            return Err(SstError::RowTooLarge);
        }
        // The head entry gets a block of its own so the chain bookkeeping
        // never shares space with packed rows.
        self.flush_data(store)?;
        let mut ids = [BlockId([0u8; 32]); MAX_CHAIN];
        for (i, chunk) in tail.chunks(MAX_PAYLOAD).enumerate() {
            ids[i] = store.put(chunk, BlockType::SstData, 0)?;
            self.record(ids[i])?;
        }
        let at = 0usize;
        self.pending[at..at + 8].copy_from_slice(&key.rowid.to_le_bytes());
        self.pending[at + 8..at + 16].copy_from_slice(&key.commit_lsn.to_le_bytes());
        self.pending[at + 16..at + 20]
            .copy_from_slice(&((row.len() as u32) | CHAIN_FLAG).to_le_bytes());
        let mut cursor = VERSIONED_ENTRY_HEADER;
        self.pending[cursor..cursor + 2].copy_from_slice(&(n_chunks as u16).to_le_bytes());
        cursor += 2;
        for id in &ids[..n_chunks] {
            self.pending[cursor..cursor + 32].copy_from_slice(&id.0);
            cursor += 32;
        }
        self.pending[cursor..cursor + head_room].copy_from_slice(&row[..head_room]);
        self.pending_len = cursor + head_room;
        self.pending_first = Some(key);
        self.last_key = Some(key);
        for filter in &mut self.filters {
            bloom::insert(filter, key.rowid);
        }
        self.key_count += 1;
        self.flush_data(store)
    }

    /// Appends a deletion marker for `rowid`. Ordered with the rows, sized
    /// like an empty entry.
    #[cfg(test)]
    pub(crate) fn append_tombstone(
        &mut self,
        store: &mut dyn BlockStore,
        rowid: u64,
    ) -> Result<(), SstError> {
        self.append_tombstone_version(store, SstKey::at(rowid, 0))
    }

    pub(crate) fn append_tombstone_version(
        &mut self,
        store: &mut dyn BlockStore,
        key: SstKey,
    ) -> Result<(), SstError> {
        if let Some(last) = self.last_key
            && key <= last
        {
            return Err(SstError::KeyOutOfOrder);
        }
        let limit = if self.pax_enabled {
            PACKED_PAX_TARGET.saturating_sub(128)
        } else {
            MAX_PAYLOAD
        };
        if self.pending_len != 0 && self.pending_len + VERSIONED_ENTRY_HEADER > limit {
            self.flush_data(store)?;
        }
        let at = self.pending_len;
        self.pending[at..at + 8].copy_from_slice(&key.rowid.to_le_bytes());
        self.pending[at + 8..at + 16].copy_from_slice(&key.commit_lsn.to_le_bytes());
        self.pending[at + 16..at + 20].copy_from_slice(&TOMB_FLAG.to_le_bytes());
        self.pending_len += VERSIONED_ENTRY_HEADER;
        if self.pending_first.is_none() {
            self.pending_first = Some(key);
        }
        self.last_key = Some(key);
        // The filter answers whether this SST *mentions* the rowid. Omitting
        // tombstones lets a negative filter skip the deletion and resurrect
        // an older member's row.
        for filter in &mut self.filters {
            bloom::insert(filter, key.rowid);
        }
        self.key_count += 1;
        Ok(())
    }

    fn record(&mut self, id: BlockId) -> Result<(), SstError> {
        if self.roster_len == MAX_ROSTER {
            return Err(SstError::TooManyBlocks);
        }
        self.roster[self.roster_len] = id;
        self.roster_len += 1;
        Ok(())
    }

    fn flush_data(&mut self, store: &mut dyn BlockStore) -> Result<(), SstError> {
        if self.pending_len == 0 {
            return Ok(());
        }
        if self.index_len == MAX_DATA_BLOCKS {
            // One index leaf is full; start another. The finish decides
            // whether a root is needed over them.
            self.flush_packed(store)?;
            self.flush_index_leaf(store)?;
        }
        let first = self
            .pending_first
            .expect("a non-empty block has a first key");
        // Store whichever of raw/LZ4 is smaller: on object storage the bytes
        // are latency, bandwidth and money, and an incompressible block
        // costs nothing but this attempt.
        let (raw, block_type) = if !self.pax_enabled {
            (&self.pending[..self.pending_len], BlockType::SstDataV2)
        } else {
            let length = self.encode_pax_v2(store)?;
            (&self.pax_buf[..length], BlockType::SstDataPaxV2)
        };
        let reference = if self.pax_enabled {
            let (id, framed_len) = super::encode(raw, block_type, 0, &mut self.compress_buf)
                .map_err(|_| SstError::PaxEncoding)?;
            if framed_len > self.packed.len() {
                DataBlockRef::direct(store.put(raw, block_type, 0)?)
            } else {
                if self.packed_len + framed_len > self.packed.len() {
                    self.flush_packed(store)?;
                }
                let offset = self.packed_len;
                self.packed[offset..offset + framed_len]
                    .copy_from_slice(&self.compress_buf[..framed_len]);
                self.packed_len += framed_len;
                DataBlockRef::Packed {
                    container: BlockId([0; 32]),
                    offset: u32::try_from(offset).map_err(|_| SstError::PaxEncoding)?,
                    length: u32::try_from(framed_len).map_err(|_| SstError::PaxEncoding)?,
                    id,
                }
            }
        } else {
            match super::lz4::compress(raw, &mut self.compress_buf[..raw.len()]) {
                Some(n) if n < raw.len() => DataBlockRef::direct(store.put(
                    &self.compress_buf[..n],
                    BlockType::SstDataV2Lz4,
                    0,
                )?),
                _ => DataBlockRef::direct(store.put(raw, block_type, 0)?),
            }
        };
        if let DataBlockRef::Direct(id) = reference {
            self.record(id)?;
        }
        self.index[self.index_len] = (first, reference);
        self.index_len += 1;
        self.pending_len = 0;
        self.pending_first = None;
        Ok(())
    }

    fn flush_packed(&mut self, store: &mut dyn BlockStore) -> Result<(), SstError> {
        if self.packed_len == 0 {
            return Ok(());
        }
        let container = store.put(
            &self.packed[..self.packed_len],
            BlockType::SstPackedContainerV1,
            0,
        )?;
        self.record(container)?;
        for (_, reference) in &mut self.index[self.packed_index_start..self.index_len] {
            if let DataBlockRef::Packed {
                container: location,
                ..
            } = reference
                && location.0 == [0; 32]
            {
                *location = container;
            }
        }
        self.packed_len = 0;
        self.packed_index_start = self.index_len;
        Ok(())
    }

    fn encode_pax_v2(&mut self, store: &mut dyn BlockStore) -> Result<usize, SstError> {
        // Descriptor frames already staged for earlier groups must receive an
        // immutable identity before this group's physical extents reuse the
        // same fixed packing buffer.
        self.flush_packed(store)?;
        let mut rows = 0usize;
        let mut payload_bytes = [0usize; MAX_COLUMNS];
        let entries = DataBlock {
            bytes: &self.pending[..self.pending_len],
        };
        for entry in entries {
            if entry.is_chained() || rows == u16::MAX as usize {
                return Err(SstError::PaxEncoding);
            }
            rows += 1;
            if entry.tombstone {
                continue;
            }
            let mut payloads = [&[][..]; MAX_COLUMNS];
            let mut nulls = [false; MAX_COLUMNS];
            rowenc::encoded_columns(
                entry.head,
                &self.pax_schema[..self.pax_columns],
                &mut payloads,
                &mut nulls,
            )
            .map_err(|_| SstError::PaxEncoding)?;
            for column in 0..self.pax_columns {
                if !nulls[column] {
                    payload_bytes[column] = payload_bytes[column]
                        .checked_add(payloads[column].len())
                        .ok_or(SstError::PaxEncoding)?;
                }
            }
        }
        let bitmap_bytes = rows.div_ceil(8);
        for column in 0..self.pax_columns {
            let column_len = 8usize
                .checked_add(payload_bytes[column])
                .ok_or(SstError::PaxEncoding)?;
            if column_len > MAX_PAYLOAD {
                return Err(SstError::PaxEncoding);
            }
            let output = &mut self.pax_buf[..column_len];
            output[..4].copy_from_slice(&PAX_COLUMN_MAGIC.to_le_bytes());
            output[4..6].copy_from_slice(&(rows as u16).to_le_bytes());
            output[6] = self.pax_schema[column].code();
            output[7] = 0;
            let mut at = 8usize;
            for entry in (DataBlock {
                bytes: &self.pending[..self.pending_len],
            }) {
                if entry.tombstone {
                    continue;
                }
                let mut payloads = [&[][..]; MAX_COLUMNS];
                let mut nulls = [false; MAX_COLUMNS];
                rowenc::encoded_columns(
                    entry.head,
                    &self.pax_schema[..self.pax_columns],
                    &mut payloads,
                    &mut nulls,
                )
                .map_err(|_| SstError::PaxEncoding)?;
                if !nulls[column] {
                    let end = at
                        .checked_add(payloads[column].len())
                        .ok_or(SstError::PaxEncoding)?;
                    output[at..end].copy_from_slice(payloads[column]);
                    at = end;
                }
            }
            if at != column_len {
                return Err(SstError::PaxEncoding);
            }
            let (id, framed_len) = super::encode(
                &self.pax_buf[..column_len],
                BlockType::SstDataPaxColumnV1,
                0,
                &mut self.compress_buf,
            )
            .map_err(|_| SstError::PaxEncoding)?;
            if self.packed_len + framed_len > self.packed.len() {
                self.flush_pax_columns(store)?;
            }
            let offset = self.packed_len;
            self.packed[offset..offset + framed_len]
                .copy_from_slice(&self.compress_buf[..framed_len]);
            self.packed_len += framed_len;
            self.pax_refs[column] = DataBlockRef::Packed {
                container: BlockId([0; 32]),
                offset: u32::try_from(offset).map_err(|_| SstError::PaxEncoding)?,
                length: u32::try_from(framed_len).map_err(|_| SstError::PaxEncoding)?,
                id,
            };
        }
        self.flush_pax_columns(store)?;

        let row_base = 8usize
            .checked_add(self.pax_columns)
            .ok_or(SstError::PaxEncoding)?;
        let bitmap_base = row_base
            .checked_add(
                rows.checked_mul(PAX_V2_ROW_HEADER)
                    .ok_or(SstError::PaxEncoding)?,
            )
            .ok_or(SstError::PaxEncoding)?;
        let refs_base = bitmap_base
            .checked_add(
                self.pax_columns
                    .checked_mul(bitmap_bytes)
                    .ok_or(SstError::PaxEncoding)?,
            )
            .ok_or(SstError::PaxEncoding)?;
        let total = refs_base
            .checked_add(
                self.pax_columns
                    .checked_mul(PACKED_DATA_REF_BYTES)
                    .ok_or(SstError::PaxEncoding)?,
            )
            .ok_or(SstError::PaxEncoding)?;
        if total > MAX_PAYLOAD {
            return Err(SstError::PaxEncoding);
        }
        let output = &mut self.pax_buf[..total];
        output[..4].copy_from_slice(&PAX_V2_MAGIC.to_le_bytes());
        output[4..6].copy_from_slice(&(rows as u16).to_le_bytes());
        output[6..8].copy_from_slice(&(self.pax_columns as u16).to_le_bytes());
        for column in 0..self.pax_columns {
            output[8 + column] = self.pax_schema[column].code();
        }
        output[bitmap_base..refs_base].fill(0);
        for (row, entry) in (DataBlock {
            bytes: &self.pending[..self.pending_len],
        })
        .enumerate()
        {
            let row_at = row_base + row * PAX_V2_ROW_HEADER;
            output[row_at..row_at + 8].copy_from_slice(&entry.key.rowid.to_le_bytes());
            output[row_at + 8..row_at + 16].copy_from_slice(&entry.key.commit_lsn.to_le_bytes());
            output[row_at + 16..row_at + 20]
                .copy_from_slice(&(if entry.tombstone { TOMB_FLAG } else { 0 }).to_le_bytes());
            let row_len = if entry.tombstone { 0 } else { entry.total_len };
            output[row_at + 20..row_at + 24].copy_from_slice(&(row_len as u32).to_le_bytes());
            if entry.tombstone {
                continue;
            }
            let mut payloads = [&[][..]; MAX_COLUMNS];
            let mut nulls = [false; MAX_COLUMNS];
            rowenc::encoded_columns(
                entry.head,
                &self.pax_schema[..self.pax_columns],
                &mut payloads,
                &mut nulls,
            )
            .map_err(|_| SstError::PaxEncoding)?;
            for column in 0..self.pax_columns {
                if nulls[column] {
                    output[bitmap_base + column * bitmap_bytes + row / 8] |= 1 << (row % 8);
                }
            }
        }
        for column in 0..self.pax_columns {
            let at = refs_base + column * PACKED_DATA_REF_BYTES;
            write_data_ref(
                self.pax_refs[column],
                &mut output[at..at + PACKED_DATA_REF_BYTES],
                true,
            );
        }
        Ok(total)
    }

    fn flush_pax_columns(&mut self, store: &mut dyn BlockStore) -> Result<(), SstError> {
        if self.packed_len == 0 {
            return Ok(());
        }
        let container = store.put(
            &self.packed[..self.packed_len],
            BlockType::SstPackedContainerV1,
            0,
        )?;
        self.record(container)?;
        for reference in &mut self.pax_refs[..self.pax_columns] {
            if let DataBlockRef::Packed {
                container: location,
                ..
            } = reference
                && location.0 == [0; 32]
            {
                *location = container;
            }
        }
        self.packed_len = 0;
        self.packed_index_start = self.index_len;
        Ok(())
    }

    /// Writes the accumulated index entries as one leaf block (the classic
    /// count-prefixed layout) and records it for the root.
    fn flush_index_leaf(&mut self, store: &mut dyn BlockStore) -> Result<(), SstError> {
        if self.index_len == 0 {
            return Ok(());
        }
        if self.leaves_len == MAX_LEAVES {
            return Err(SstError::TooManyBlocks);
        }
        let entry_size = if self.pax_enabled {
            PACKED_VERSIONED_INDEX_ENTRY
        } else {
            VERSIONED_INDEX_ENTRY
        };
        let bytes = 4 + self.index_len * entry_size;
        let buffer = &mut *self.compress_buf; // free between data flushes
        buffer[0..4].copy_from_slice(&(self.index_len as u32).to_le_bytes());
        for (i, (first, id)) in self.index[..self.index_len].iter().enumerate() {
            let at = 4 + i * entry_size;
            write_key(*first, &mut buffer[at..at + 16]);
            write_data_ref(*id, &mut buffer[at + 16..at + entry_size], self.pax_enabled);
        }
        let id = store.put(&self.compress_buf[..bytes], BlockType::SstIndexV2, 0)?;
        self.record(id)?;
        self.leaves[self.leaves_len] = (self.index[0].0, self.index_len as u32, id);
        self.leaves_len += 1;
        self.index_len = 0;
        self.packed_index_start = 0;
        Ok(())
    }

    /// Flushes the last data block and writes the index. Returns the index
    /// block's identity — the SST's root — or `None` when no rows were written,
    /// since an empty SST has no root to name.
    pub(crate) fn finish(
        &mut self,
        store: &mut dyn BlockStore,
    ) -> Result<Option<SstHandle>, SstError> {
        self.flush_data(store)?;
        self.flush_packed(store)?;
        if self.index_len == 0 && self.leaves_len == 0 {
            return Ok(None);
        }
        // The filter block, so a reader can skip this SST without the
        // index — the smallest ladder tier still giving ~10 bits per key.
        let tier = self
            .filters
            .iter()
            .position(|f| self.key_count * 10 <= f.len() * 8)
            .unwrap_or(self.filters.len() - 1);
        let filter = store.put(&self.filters[tier], BlockType::SstFilter, 0)?;
        self.record(filter)?;
        // The index. One leaf's worth of entries makes the classic single
        // block; more make leaves under a root, so SST size is no longer
        // bounded by one index block.
        let index = if self.leaves_len == 0 {
            let bytes = 4 + self.index_len
                * if self.pax_enabled {
                    PACKED_VERSIONED_INDEX_ENTRY
                } else {
                    VERSIONED_INDEX_ENTRY
                };
            let buffer = &mut *self.pending; // reuse the data scratch; it is done with
            buffer[0..4].copy_from_slice(&(self.index_len as u32).to_le_bytes());
            for (i, (first, id)) in self.index[..self.index_len].iter().enumerate() {
                let entry_size = if self.pax_enabled {
                    PACKED_VERSIONED_INDEX_ENTRY
                } else {
                    VERSIONED_INDEX_ENTRY
                };
                let at = 4 + i * entry_size;
                write_key(*first, &mut buffer[at..at + 16]);
                write_data_ref(*id, &mut buffer[at + 16..at + entry_size], self.pax_enabled);
            }
            store.put(&buffer[..bytes], BlockType::SstIndexV2, 0)?
        } else {
            self.flush_index_leaf(store)?;
            let bytes = 8 + self.leaves_len * VERSIONED_ROOT_ENTRY;
            let buffer = &mut *self.pending;
            buffer[0..4].copy_from_slice(&INDEX_ROOT_MAGIC.to_le_bytes());
            buffer[4..8].copy_from_slice(&(self.leaves_len as u32).to_le_bytes());
            for (i, (first, count, id)) in self.leaves[..self.leaves_len].iter().enumerate() {
                let at = 8 + i * VERSIONED_ROOT_ENTRY;
                write_key(*first, &mut buffer[at..at + 16]);
                buffer[at + 16..at + 20].copy_from_slice(&count.to_le_bytes());
                buffer[at + 20..at + VERSIONED_ROOT_ENTRY].copy_from_slice(&id.0);
            }
            store.put(&buffer[..bytes], BlockType::SstIndexV2, 0)?
        };
        if self.roster_len == MAX_ROSTER {
            return Err(SstError::TooManyBlocks);
        }
        self.roster[self.roster_len] = index;
        self.roster_len += 1;
        // The roster last: every identity this SST comprises, so a sweeper
        // enumerates the SST by one read. It cannot list itself — its own
        // identity is a hash of its contents — so the sweeper keeps the
        // roster alive through the handle that names it.
        let roster_bytes = self.roster_len * 32;
        let buffer = &mut *self.pending;
        for (i, id) in self.roster[..self.roster_len].iter().enumerate() {
            buffer[i * 32..i * 32 + 32].copy_from_slice(&id.0);
        }
        let roster = store.put(&buffer[..roster_bytes], BlockType::SstRoster, 0)?;
        Ok(Some(SstHandle {
            index,
            filter,
            roster,
            packed: self.pax_enabled,
        }))
    }
}

/// Reads rows out of an SST by its root. Holds one block of scratch for the
/// index and one for a data block, so a lookup borrows no memory from the
/// caller beyond the buffer the row is copied into.
pub(crate) struct SstReader<'a> {
    index_scratch: &'a mut [u8],
    data_scratch: &'a mut [u8],
    decoded_scratch: &'a mut [u8],
    column_scratch: &'a mut [u8],
    loaded_ref: Option<DataBlockRef>,
    decoded_len: usize,
    /// Scratch a range scan assembles a chained row into (a point lookup
    /// assembles straight into the caller's buffer instead).
    assembly: &'a mut [u8],
}

/// Allocation-free, suspendable traversal of one SST in key order.
///
/// [`SstReader::scan`] is deliberately callback-shaped for storage walks, but
/// an external merge needs one current row from several runs at once. This
/// cursor keeps only the data-block ordinal and byte offset; all buffers are
/// caller-owned startup scratch, and every advance still reads through the
/// same [`BlockStore`] cache stack.
#[derive(Clone, Copy)]
pub(crate) struct SstCursor {
    handle: SstHandle,
    block_ordinal: usize,
    offset: usize,
    data_len: usize,
    loaded: bool,
    prefetched_leaf: Option<(usize, BlockId)>,
    prefetched_data: Option<(usize, DataBlockRef)>,
    done: bool,
}

impl SstCursor {
    pub(crate) fn new(handle: SstHandle) -> Self {
        Self {
            handle,
            block_ordinal: 0,
            offset: 0,
            data_len: 0,
            loaded: false,
            prefetched_leaf: None,
            prefetched_data: None,
            done: false,
        }
    }

    /// Copies the next live row into `out`, returning `(key, length)`.
    ///
    /// External execution never writes tombstones and caps one projected row
    /// at one block payload. Encountering either a tombstone or a chained row
    /// is therefore corruption of an execution run, not an alternate result.
    pub(crate) fn next_copy(
        &mut self,
        store: &mut dyn BlockStore,
        index: &mut [u8],
        data: &mut [u8],
        bounce: &mut [u8],
        out: &mut [u8],
    ) -> Result<Option<(u64, usize)>, SstError> {
        loop {
            if self.done {
                return Ok(None);
            }
            self.advance_lookahead(store, index)?;
            if !self.loaded || self.offset >= self.data_len {
                let id = if let Some((ordinal, id)) = self.prefetched_data
                    && ordinal == self.block_ordinal
                {
                    self.prefetched_data = None;
                    id
                } else {
                    let Some((id, next)) = locate_data_block_with_next(
                        store,
                        &self.handle,
                        index,
                        self.block_ordinal,
                    )?
                    else {
                        self.done = true;
                        return Ok(None);
                    };
                    self.schedule_lookahead(store, self.block_ordinal + 1, next)?;
                    id
                };
                self.data_len = read_external_run_data_block_ref(store, id, data, bounce)?;
                self.block_ordinal += 1;
                self.offset = 0;
                self.loaded = true;
                if self.data_len == 0 {
                    continue;
                }
            }

            let remaining = &data[self.offset..self.data_len];
            let before = remaining.len();
            let mut entries = DataBlock { bytes: remaining };
            let Some(entry) = entries.next() else {
                return Err(SstError::Store(StoreError::Corrupt(
                    super::BlockError::Truncated,
                )));
            };
            self.offset += before - entries.bytes.len();
            if entry.tombstone || entry.is_chained() || entry.total_len > out.len() {
                return Err(if entry.total_len > out.len() {
                    SstError::RowTooLarge
                } else {
                    SstError::Store(StoreError::Corrupt(super::BlockError::Payload))
                });
            }
            out[..entry.total_len].copy_from_slice(entry.head);
            return Ok(Some((entry.key.rowid, entry.total_len)));
        }
    }

    fn schedule_lookahead(
        &mut self,
        store: &mut dyn BlockStore,
        ordinal: usize,
        next: Option<DataBlockLookahead>,
    ) -> Result<(), SstError> {
        match next {
            Some(DataBlockLookahead::Data(reference)) => {
                if let DataBlockRef::Direct(id) = reference {
                    prefetch_data_block(store, Some(id))?;
                    self.prefetched_data = Some((ordinal, reference));
                }
            }
            Some(DataBlockLookahead::Leaf(id)) => {
                prefetch_index_block(store, id)?;
                self.prefetched_leaf = Some((ordinal, id));
            }
            None => {}
        }
        Ok(())
    }

    fn advance_lookahead(
        &mut self,
        store: &mut dyn BlockStore,
        index: &mut [u8],
    ) -> Result<(), SstError> {
        let Some((ordinal, leaf)) = self.prefetched_leaf else {
            return Ok(());
        };
        let Some(reference) =
            take_prefetched_index_first_data(store, &leaf, index, self.handle.packed)?
        else {
            return Ok(());
        };
        self.prefetched_leaf = None;
        if let DataBlockRef::Direct(id) = reference {
            prefetch_data_block(store, Some(id))?;
            self.prefetched_data = Some((ordinal, reference));
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(crate) enum DataBlockLookahead {
    Data(DataBlockRef),
    Leaf(BlockId),
}

/// One SST's best version for a snapshot. `len == None` is a deletion marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SstProbe {
    pub(crate) key: SstKey,
    pub(crate) len: Option<u32>,
}

/// Fetches a data block, transparently decompressing an LZ4 one: `buf`
/// receives the raw entries either way; `bounce` stages the compressed
/// bytes (any MAX_PAYLOAD-sized buffer that is free at the call).
pub(crate) fn read_data_block(
    store: &mut dyn BlockStore,
    id: &BlockId,
    buf: &mut [u8],
    bounce: &mut [u8],
) -> Result<usize, SstError> {
    read_data_block_with_type(store, id, buf, bounce, None).map(|(length, _, _)| length)
}

/// Decodes a canonical row block belonging to an external execution run.
///
/// PAX descriptors require their column extents and are intentionally handled
/// only by the PAX-aware reader paths below; an execution run cannot contain
/// one.
pub(crate) fn read_external_run_data_block_ref(
    store: &mut dyn BlockStore,
    reference: DataBlockRef,
    buf: &mut [u8],
    bounce: &mut [u8],
) -> Result<usize, SstError> {
    match reference {
        DataBlockRef::Direct(id) => read_data_block(store, &id, buf, bounce),
        DataBlockRef::Packed {
            container,
            offset,
            length,
            id,
        } => {
            let (n, block_type) = store.get_packed(
                &container,
                offset as usize,
                length as usize,
                &id,
                buf,
                bounce,
            )?;
            if bounce.len() < n {
                return Err(SstError::Store(StoreError::BufferTooSmall));
            }
            bounce[..n].copy_from_slice(&buf[..n]);
            decode_data_block(&bounce[..n], block_type, buf)
        }
    }
}

/// Loads a data block into canonical row bytes and reports its durable physical
/// layout. A scan can retain this state while it decides whether a later PAX
/// path can consume column groups directly.
pub(crate) fn read_data_block_with_type(
    store: &mut dyn BlockStore,
    id: &BlockId,
    buf: &mut [u8],
    bounce: &mut [u8],
    raw: Option<&mut [u8]>,
) -> Result<(usize, BlockType, usize), SstError> {
    let (n, block_type) = read_data_block_raw(store, id, buf)?;
    if let Some(raw) = raw {
        if raw.len() < n {
            return Err(SstError::Store(StoreError::Corrupt(
                super::BlockError::Payload,
            )));
        }
        raw[..n].copy_from_slice(&buf[..n]);
    }
    if bounce.len() < n {
        return Err(SstError::Store(StoreError::Corrupt(
            super::BlockError::Payload,
        )));
    }
    bounce[..n].copy_from_slice(&buf[..n]);
    decode_data_block(&bounce[..n], block_type, buf).map(|length| (length, block_type, n))
}

/// Canonicalizes already-fetched physical data-block bytes. Keeping this
/// separate from the demand read lets a PAX-aware merge cursor retain PAX in
/// its column layout while ordinary block formats still use the same decoder.
pub(crate) fn decode_data_block(
    input: &[u8],
    block_type: BlockType,
    output: &mut [u8],
) -> Result<usize, SstError> {
    match block_type {
        BlockType::SstDataV2 => {
            if output.len() < input.len() {
                return Err(SstError::Store(StoreError::Corrupt(
                    super::BlockError::Payload,
                )));
            }
            output[..input.len()].copy_from_slice(input);
            Ok(input.len())
        }
        BlockType::SstDataV2Lz4 => super::lz4::decompress(input, output).ok_or(SstError::Store(
            StoreError::Corrupt(super::BlockError::Payload),
        )),
        _ => Err(SstError::Store(StoreError::Corrupt(
            super::BlockError::UnknownType,
        ))),
    }
}

/// Loads an immutable data block without changing its physical layout.  Callers
/// that need the canonical row stream use [`read_data_block`]; a PAX-aware scan
/// uses this choke point and validates the group before touching its columns.
pub(crate) fn read_data_block_raw(
    store: &mut dyn BlockStore,
    id: &BlockId,
    buf: &mut [u8],
) -> Result<(usize, BlockType), SstError> {
    let (n, block_type) = store.get(id, buf)?;
    match block_type {
        BlockType::SstDataV2
        | BlockType::SstDataV2Lz4
        | BlockType::SstDataPaxV2
        | BlockType::SstDataPaxColumnV1 => Ok((n, block_type)),
        _ => Err(SstError::Store(StoreError::Corrupt(
            super::BlockError::UnknownType,
        ))),
    }
}

pub(crate) fn read_data_block_raw_ref(
    store: &mut dyn BlockStore,
    reference: DataBlockRef,
    buf: &mut [u8],
    scratch: &mut [u8],
) -> Result<(usize, BlockType), SstError> {
    match reference {
        DataBlockRef::Direct(id) => read_data_block_raw(store, &id, buf),
        DataBlockRef::Packed {
            container,
            offset,
            length,
            id,
        } => store
            .get_packed(
                &container,
                offset as usize,
                length as usize,
                &id,
                buf,
                scratch,
            )
            .map_err(SstError::Store),
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PaxLayout {
    rows: usize,
    columns: usize,
    row_base: usize,
    row_header: usize,
    bitmap_base: usize,
    bitmap_bytes: usize,
    schema: [ColType; MAX_COLUMNS],
    refs_base: usize,
}

impl PaxLayout {
    pub(crate) fn rows(&self) -> usize {
        self.rows
    }

    pub(crate) fn columns(&self) -> usize {
        self.columns
    }

    pub(crate) fn column_ref(&self, input: &[u8], column: usize) -> Result<DataBlockRef, SstError> {
        if column >= self.columns {
            return Err(SstError::Store(StoreError::Corrupt(
                super::BlockError::Payload,
            )));
        }
        let at = self
            .refs_base
            .checked_add(column * PACKED_DATA_REF_BYTES)
            .ok_or(SstError::Store(StoreError::Corrupt(
                super::BlockError::Payload,
            )))?;
        let bytes = input
            .get(at..at + PACKED_DATA_REF_BYTES)
            .ok_or(SstError::Store(StoreError::Corrupt(
                super::BlockError::Payload,
            )))?;
        Ok(read_data_ref(bytes, true))
    }

    pub(crate) fn row_len(&self, input: &[u8], row: usize) -> Result<u32, SstError> {
        if row >= self.rows {
            return Err(SstError::Store(StoreError::Corrupt(
                super::BlockError::Payload,
            )));
        }
        let at = self.row_base + row * self.row_header + PAX_ROW_HEADER;
        Ok(u32::from_le_bytes(input[at..at + 4].try_into().unwrap()))
    }

    pub(crate) fn row_key(&self, input: &[u8], row: usize) -> Result<(SstKey, bool), SstError> {
        let corrupt = || SstError::Store(StoreError::Corrupt(super::BlockError::Payload));
        if row >= self.rows {
            return Err(corrupt());
        }
        let at = self.row_base + row * self.row_header;
        let rowid = u64::from_le_bytes(input[at..at + 8].try_into().unwrap());
        let commit_lsn = u64::from_le_bytes(input[at + 8..at + 16].try_into().unwrap());
        let flags = u32::from_le_bytes(input[at + 16..at + 20].try_into().unwrap());
        if flags & !TOMB_FLAG != 0 {
            return Err(corrupt());
        }
        Ok((SstKey::at(rowid, commit_lsn), flags & TOMB_FLAG != 0))
    }

    pub(crate) fn column_is_null(
        &self,
        input: &[u8],
        row: usize,
        column: usize,
    ) -> Result<bool, SstError> {
        let corrupt = || SstError::Store(StoreError::Corrupt(super::BlockError::Payload));
        if row >= self.rows || column >= self.columns {
            return Err(corrupt());
        }
        Ok(input[self.bitmap_base + column * self.bitmap_bytes + row / 8] & (1 << (row % 8)) != 0)
    }
}

pub(crate) fn pax_layout(input: &[u8]) -> Result<PaxLayout, SstError> {
    let corrupt = || SstError::Store(StoreError::Corrupt(super::BlockError::Payload));
    if input.len() < 8 {
        return Err(corrupt());
    }
    let magic = u32::from_le_bytes(input[..4].try_into().unwrap());
    if magic != PAX_V2_MAGIC {
        return Err(corrupt());
    }
    let rows = u16::from_le_bytes(input[4..6].try_into().unwrap()) as usize;
    let columns = u16::from_le_bytes(input[6..8].try_into().unwrap()) as usize;
    if columns > MAX_COLUMNS {
        return Err(corrupt());
    }
    let row_header = PAX_V2_ROW_HEADER;
    let bitmap_bytes = rows.div_ceil(8);
    let row_base = 8usize.checked_add(columns).ok_or_else(corrupt)?;
    let bitmap_base = row_base
        .checked_add(rows.checked_mul(row_header).ok_or_else(corrupt)?)
        .ok_or_else(corrupt)?;
    let values_base = bitmap_base
        .checked_add(columns.checked_mul(bitmap_bytes).ok_or_else(corrupt)?)
        .ok_or_else(corrupt)?;
    if values_base > input.len() {
        return Err(corrupt());
    }
    let mut schema = [ColType::Bool; MAX_COLUMNS];
    for column in 0..columns {
        schema[column] = ColType::from_code(input[8 + column]).ok_or_else(corrupt)?;
    }
    let refs_end = values_base
        .checked_add(
            columns
                .checked_mul(PACKED_DATA_REF_BYTES)
                .ok_or_else(corrupt)?,
        )
        .ok_or_else(corrupt)?;
    if refs_end != input.len() {
        return Err(corrupt());
    }
    for row in 0..rows {
        let flags_at = row_base + row * row_header + 16;
        let flags = u32::from_le_bytes(input[flags_at..flags_at + 4].try_into().unwrap());
        if flags & !TOMB_FLAG != 0 {
            return Err(corrupt());
        }
        let len_at = flags_at + 4;
        let len = u32::from_le_bytes(input[len_at..len_at + 4].try_into().unwrap());
        if flags & TOMB_FLAG != 0 {
            if len != 0 {
                return Err(corrupt());
            }
        } else if !(2 + columns.div_ceil(8)..=MAX_INLINE_ROW).contains(&(len as usize)) {
            return Err(corrupt());
        }
    }
    Ok(PaxLayout {
        rows,
        columns,
        row_base,
        row_header,
        bitmap_base,
        bitmap_bytes,
        schema,
        refs_base: values_base,
    })
}

fn decode_pax_v2(
    store: &mut dyn BlockStore,
    input: &[u8],
    output: &mut [u8],
    column_scratch: &mut [u8],
    range_scratch: &mut [u8],
) -> Result<usize, SstError> {
    let corrupt = || SstError::Store(StoreError::Corrupt(super::BlockError::Payload));
    let layout = pax_layout(input)?;
    let mut extents = [None; MAX_COLUMNS];
    let mut extent_len = 0usize;
    for (column, extent) in extents.iter_mut().enumerate().take(layout.columns()) {
        let reference = layout.column_ref(input, column)?;
        let (column_len, block_type) =
            read_data_block_raw_ref(store, reference, column_scratch, output)?;
        if block_type != BlockType::SstDataPaxColumnV1 || column_len < 8 {
            return Err(corrupt());
        }
        let end = extent_len.checked_add(column_len).ok_or_else(corrupt)?;
        if end > range_scratch.len() {
            return Err(SstError::RowTooLarge);
        }
        range_scratch[extent_len..end].copy_from_slice(&column_scratch[..column_len]);
        *extent = Some((extent_len, end));
        extent_len = end;
    }
    let mut written = 0usize;
    for row in 0..layout.rows() {
        let (key, tombstone) = layout.row_key(input, row)?;
        let row_len = layout.row_len(input, row)? as usize;
        let header_end = written
            .checked_add(VERSIONED_ENTRY_HEADER)
            .ok_or_else(corrupt)?;
        if header_end > output.len() {
            return Err(corrupt());
        }
        output[written..written + 8].copy_from_slice(&key.rowid.to_le_bytes());
        output[written + 8..written + 16].copy_from_slice(&key.commit_lsn.to_le_bytes());
        output[written + 16..header_end]
            .copy_from_slice(&(if tombstone { TOMB_FLAG } else { row_len as u32 }).to_le_bytes());
        written = header_end;
        if tombstone {
            continue;
        }
        let row_end = written.checked_add(row_len).ok_or_else(corrupt)?;
        if row_end > output.len() {
            return Err(corrupt());
        }
        let copied = copy_pax_v2_row_from_extents(
            &layout,
            input,
            row,
            None,
            &range_scratch[..extent_len],
            &extents,
            &mut output[written..row_end],
        )?;
        if copied != row_len {
            return Err(corrupt());
        }
        written = row_end;
    }
    Ok(written)
}

#[cfg(test)]
pub(crate) struct PaxReadScratch<'a> {
    pub(crate) column: &'a mut [u8],
    pub(crate) range: &'a mut [u8],
}

#[cfg(test)]
pub(crate) fn copy_pax_v2_row_demand(
    store: &mut dyn BlockStore,
    layout: &PaxLayout,
    input: &[u8],
    row: usize,
    demanded: Option<&[bool; MAX_COLUMNS]>,
    scratch: &mut PaxReadScratch<'_>,
    output: &mut [u8],
) -> Result<usize, SstError> {
    let corrupt = || SstError::Store(StoreError::Corrupt(super::BlockError::Payload));
    let (_, tombstone) = layout.row_key(input, row)?;
    if tombstone {
        return Err(corrupt());
    }
    let bitmap_bytes = layout.columns().div_ceil(8);
    let mut written = 2usize.checked_add(bitmap_bytes).ok_or_else(corrupt)?;
    if written > output.len() {
        return Err(corrupt());
    }
    output[..2].copy_from_slice(&(layout.columns() as u16).to_le_bytes());
    output[2..written].fill(0);
    let all_columns = demanded.is_none();
    for column in 0..layout.columns() {
        if demanded.is_some_and(|columns| !columns[column])
            || layout.column_is_null(input, row, column)?
        {
            output[2 + column / 8] |= 1 << (column % 8);
            continue;
        }
        let reference = layout.column_ref(input, column)?;
        let (column_len, block_type) =
            read_data_block_raw_ref(store, reference, scratch.column, scratch.range)?;
        if block_type != BlockType::SstDataPaxColumnV1 || column_len < 8 {
            return Err(corrupt());
        }
        if u32::from_le_bytes(scratch.column[..4].try_into().unwrap()) != PAX_COLUMN_MAGIC
            || u16::from_le_bytes(scratch.column[4..6].try_into().unwrap()) as usize
                != layout.rows()
            || ColType::from_code(scratch.column[6]) != Some(layout.schema[column])
            || scratch.column[7] != 0
        {
            return Err(corrupt());
        }
        let mut at = 8usize;
        for preceding in 0..row {
            let (_, preceding_tombstone) = layout.row_key(input, preceding)?;
            if preceding_tombstone || layout.column_is_null(input, preceding, column)? {
                continue;
            }
            let length =
                rowenc::encoded_value_len(&scratch.column[at..column_len], layout.schema[column])
                    .map_err(|_| corrupt())?;
            at = at.checked_add(length).ok_or_else(corrupt)?;
            if at > column_len {
                return Err(corrupt());
            }
        }
        let length =
            rowenc::encoded_value_len(&scratch.column[at..column_len], layout.schema[column])
                .map_err(|_| corrupt())?;
        let end = at.checked_add(length).ok_or_else(corrupt)?;
        let next = written.checked_add(length).ok_or_else(corrupt)?;
        if end > column_len || next > output.len() {
            return Err(corrupt());
        }
        output[written..next].copy_from_slice(&scratch.column[at..end]);
        written = next;
    }
    if all_columns && written != layout.row_len(input, row)? as usize {
        return Err(corrupt());
    }
    Ok(written)
}

pub(crate) fn copy_pax_v2_row_from_extents(
    layout: &PaxLayout,
    input: &[u8],
    row: usize,
    demanded: Option<&[bool; MAX_COLUMNS]>,
    extent_bytes: &[u8],
    extents: &[Option<(usize, usize)>; MAX_COLUMNS],
    output: &mut [u8],
) -> Result<usize, SstError> {
    let corrupt = || SstError::Store(StoreError::Corrupt(super::BlockError::Payload));
    let (_, tombstone) = layout.row_key(input, row)?;
    if tombstone {
        return Err(corrupt());
    }
    let bitmap_bytes = layout.columns().div_ceil(8);
    let mut written = 2usize.checked_add(bitmap_bytes).ok_or_else(corrupt)?;
    if written > output.len() {
        return Err(corrupt());
    }
    output[..2].copy_from_slice(&(layout.columns() as u16).to_le_bytes());
    output[2..written].fill(0);
    let all_columns = demanded.is_none();
    for column in 0..layout.columns() {
        if demanded.is_some_and(|columns| !columns[column])
            || layout.column_is_null(input, row, column)?
        {
            output[2 + column / 8] |= 1 << (column % 8);
            continue;
        }
        let (start, end) = extents[column].ok_or_else(corrupt)?;
        let bytes = extent_bytes.get(start..end).ok_or_else(corrupt)?;
        if bytes.len() < 8
            || u32::from_le_bytes(bytes[..4].try_into().unwrap()) != PAX_COLUMN_MAGIC
            || u16::from_le_bytes(bytes[4..6].try_into().unwrap()) as usize != layout.rows()
            || ColType::from_code(bytes[6]) != Some(layout.schema[column])
            || bytes[7] != 0
        {
            return Err(corrupt());
        }
        let mut at = 8usize;
        for preceding in 0..row {
            let (_, preceding_tombstone) = layout.row_key(input, preceding)?;
            if preceding_tombstone || layout.column_is_null(input, preceding, column)? {
                continue;
            }
            let length = rowenc::encoded_value_len(&bytes[at..], layout.schema[column])
                .map_err(|_| corrupt())?;
            at = at.checked_add(length).ok_or_else(corrupt)?;
            if at > bytes.len() {
                return Err(corrupt());
            }
        }
        let length = rowenc::encoded_value_len(&bytes[at..], layout.schema[column])
            .map_err(|_| corrupt())?;
        let end = at.checked_add(length).ok_or_else(corrupt)?;
        let next = written.checked_add(length).ok_or_else(corrupt)?;
        if end > bytes.len() || next > output.len() {
            return Err(corrupt());
        }
        output[written..next].copy_from_slice(&bytes[at..end]);
        written = next;
    }
    if all_columns && written != layout.row_len(input, row)? as usize {
        return Err(corrupt());
    }
    Ok(written)
}

/// Starts a read whose bytes a sequential scan will need next. The store
/// retains ownership of a completed body until the demand read consumes it.
pub(crate) fn prefetch_data_block(
    store: &mut dyn BlockStore,
    id: Option<BlockId>,
) -> Result<(), SstError> {
    let Some(id) = id else { return Ok(()) };
    if !store.async_gets_enabled() {
        return Ok(());
    }
    match store.prefetch(&id).map_err(SstError::Store)? {
        super::PrefetchState::Scheduled
        | super::PrefetchState::Reused
        | super::PrefetchState::Saturated => Ok(()),
        super::PrefetchState::Unavailable => Err(SstError::Store(StoreError::Unavailable)),
    }
}

fn prefetch_index_block(store: &mut dyn BlockStore, id: BlockId) -> Result<(), SstError> {
    if !store.async_gets_enabled() {
        return Ok(());
    }
    match store.prefetch(&id).map_err(SstError::Store)? {
        super::PrefetchState::Scheduled
        | super::PrefetchState::Reused
        | super::PrefetchState::Saturated => Ok(()),
        super::PrefetchState::Unavailable => Err(SstError::Store(StoreError::Unavailable)),
    }
}

/// Schedules consecutive entries from an already resident SST index leaf.
/// The current block is deliberately excluded: it remains the demand read,
/// leaving one fixed slot available for it. A saturated scheduler stops this
/// optional window immediately; the saturation counter records that decision.
fn prefetch_data_window(
    store: &mut dyn BlockStore,
    index: &[u8],
    first: usize,
    count: usize,
    packed: bool,
) -> Result<(), SstError> {
    if packed || !store.async_gets_enabled() {
        return Ok(());
    }
    let slots = store.async_read_slots();
    if slots <= 1 {
        return Ok(());
    }
    let end = first.saturating_add(slots - 1).min(count);
    for entry in first..end {
        match store
            .prefetch(&block_ref_at(index, entry, false).id())
            .map_err(SstError::Store)?
        {
            super::PrefetchState::Scheduled | super::PrefetchState::Reused => {}
            super::PrefetchState::Saturated => break,
            super::PrefetchState::Unavailable => {
                return Err(SstError::Store(StoreError::Unavailable));
            }
        }
    }
    Ok(())
}

/// Decodes one completed prefetched index leaf and returns its first data
/// block identity. The transfer is confined to the scheduler's named request.
pub(crate) fn take_prefetched_index_first_data(
    store: &mut dyn BlockStore,
    id: &BlockId,
    into: &mut [u8],
    packed: bool,
) -> Result<Option<DataBlockRef>, SstError> {
    let Some((_, block_type)) = store.take_prefetch(id, into)? else {
        return Ok(None);
    };
    validate_index_type(block_type)?;
    Ok(Some(block_ref_at(into, 0, packed)))
}

impl<'a> SstReader<'a> {
    /// Restores a canonical block held by the caller-owned scratch. The
    /// caller retains that scratch for its entire lifetime, so its decoded
    /// bytes remain valid exactly while this identity does.
    pub(crate) fn restore_cached_data_block(&mut self, cached: Option<(DataBlockRef, usize)>) {
        if let Some((reference, len)) = cached {
            self.loaded_ref = Some(reference);
            self.decoded_len = len;
        }
    }

    pub(crate) fn cached_data_block(&self) -> Option<(DataBlockRef, usize)> {
        self.loaded_ref
            .map(|reference| (reference, self.decoded_len))
    }

    pub(crate) fn new(arena: &'a Arena) -> Result<Self, SstError> {
        let index_scratch = arena
            .alloc_slice_with(MAX_PAYLOAD, |_| 0u8)
            .map_err(|_| SstError::Store(StoreError::Unavailable))?;
        let data_scratch = arena
            .alloc_slice_with(MAX_PAYLOAD, |_| 0u8)
            .map_err(|_| SstError::Store(StoreError::Unavailable))?;
        let decoded_scratch = arena
            .alloc_slice_with(MAX_PAYLOAD, |_| 0u8)
            .map_err(|_| SstError::Store(StoreError::Unavailable))?;
        let column_scratch = arena
            .alloc_slice_with(MAX_PAYLOAD, |_| 0u8)
            .map_err(|_| SstError::Store(StoreError::Unavailable))?;
        let assembly = arena
            .alloc_slice_with(MAX_ASSEMBLED, |_| 0u8)
            .map_err(|_| SstError::Store(StoreError::Unavailable))?;
        Ok(Self {
            index_scratch,
            data_scratch,
            decoded_scratch,
            column_scratch,
            loaded_ref: None,
            decoded_len: 0,
            assembly,
        })
    }

    /// A reader over caller-owned buffers — the long-lived spill path, whose
    /// scratch persists across statements instead of living in an arena.
    /// `index`, `data`, `decoded`, and `column` must each hold a block payload;
    /// `assembly` holds a chained row or a packed-range scratch extent.
    pub(crate) fn over(
        index: &'a mut [u8],
        data: &'a mut [u8],
        decoded: &'a mut [u8],
        column: &'a mut [u8],
        assembly: &'a mut [u8],
    ) -> Self {
        Self {
            index_scratch: index,
            data_scratch: data,
            decoded_scratch: decoded,
            column_scratch: column,
            loaded_ref: None,
            decoded_len: 0,
            assembly,
        }
    }

    fn load_data_block(
        &mut self,
        store: &mut dyn BlockStore,
        reference: DataBlockRef,
    ) -> Result<usize, SstError> {
        if self.loaded_ref == Some(reference) {
            return Ok(self.decoded_len);
        }
        let (raw_len, block_type) =
            read_data_block_raw_ref(store, reference, self.data_scratch, self.assembly)?;
        let decoded_len = if block_type == BlockType::SstDataPaxV2 {
            decode_pax_v2(
                store,
                &self.data_scratch[..raw_len],
                self.decoded_scratch,
                self.column_scratch,
                self.assembly,
            )?
        } else {
            decode_data_block(
                &self.data_scratch[..raw_len],
                block_type,
                self.decoded_scratch,
            )?
        };
        self.loaded_ref = Some(reference);
        self.decoded_len = decoded_len;
        Ok(decoded_len)
    }

    /// Finds `rowid`, copying its row into `into` and returning the length, or
    /// `None` when the SST does not hold it. Checks the filter first: a key the
    /// filter rejects returns without the index or a data block being read at
    /// all. A key it admits reads the index and the one data block the key
    /// would be in — two blocks, as before, plus the filter.
    #[cfg(test)]
    pub(crate) fn get(
        &mut self,
        store: &mut dyn BlockStore,
        handle: &SstHandle,
        rowid: u64,
        into: &mut [u8],
    ) -> Result<Option<usize>, SstError> {
        Ok(self
            .get_at(store, handle, rowid, u64::MAX, into)?
            .and_then(|probe| probe.len.map(|length| length as usize)))
    }

    /// Finds the newest version of `rowid` whose commit LSN is at or below
    /// `snapshot`, copying a live image into `into`. A returned `SstProbe`
    /// with no length is a visible tombstone, distinct from no version.
    pub(crate) fn get_at(
        &mut self,
        store: &mut dyn BlockStore,
        handle: &SstHandle,
        rowid: u64,
        snapshot: u64,
        into: &mut [u8],
    ) -> Result<Option<SstProbe>, SstError> {
        // The filter reuses the index buffer: it is consulted and done with
        // before the index is read, so the two never coexist.
        let (filter_len, _) = store.get(&handle.filter, self.index_scratch)?;
        if !bloom::maybe_contains(&self.index_scratch[..filter_len], rowid) {
            return Ok(None);
        }
        let target = SstKey::at(rowid, snapshot);
        let count = self.load_covering_leaf(store, handle, target)?;
        let Some(entry) = block_containing(self.index_scratch, count, target, handle.packed) else {
            return Ok(None);
        };
        let block_ref = block_ref_at(self.index_scratch, entry, handle.packed);

        // Scan the one data block for the row. The block is small and bounded,
        // so a linear scan of it is the read the sparse index traded for not
        // indexing every row.
        let data_len = self.load_data_block(store, block_ref)?;
        for entry in (DataBlock {
            bytes: &self.decoded_scratch[..data_len],
        }) {
            if entry.key.rowid == rowid && entry.key.commit_lsn <= snapshot {
                if entry.tombstone {
                    return Ok(Some(SstProbe {
                        key: entry.key,
                        len: None,
                    }));
                }
                if entry.is_chained() {
                    assemble_chain(store, &entry, into)?;
                } else {
                    if into.len() < entry.total_len {
                        return Err(SstError::Store(StoreError::BufferTooSmall));
                    }
                    into[..entry.total_len].copy_from_slice(entry.head);
                }
                return Ok(Some(SstProbe {
                    key: entry.key,
                    len: Some(entry.total_len as u32),
                }));
            }
            if entry.key.rowid > rowid {
                break;
            }
        }
        Ok(None)
    }

    /// Whether the SST holds `rowid`, without copying its bytes: `None` —
    /// absent; `Some(None)` — a tombstone; `Some(Some(len))` — a live row of
    /// `len` bytes. The filter and index gate the read exactly as `get`
    /// does; this is the existence probe the row-map overlay answers point
    /// lookups with.
    #[cfg(test)]
    pub(crate) fn probe(
        &mut self,
        store: &mut dyn BlockStore,
        handle: &SstHandle,
        rowid: u64,
    ) -> Result<Option<Option<u32>>, SstError> {
        Ok(self
            .probe_at(store, handle, rowid, u64::MAX)?
            .map(|probe| probe.len))
    }

    pub(crate) fn probe_at(
        &mut self,
        store: &mut dyn BlockStore,
        handle: &SstHandle,
        rowid: u64,
        snapshot: u64,
    ) -> Result<Option<SstProbe>, SstError> {
        let (filter_len, _) = store.get(&handle.filter, self.index_scratch)?;
        if !bloom::maybe_contains(&self.index_scratch[..filter_len], rowid) {
            return Ok(None);
        }
        let target = SstKey::at(rowid, snapshot);
        let count = self.load_covering_leaf(store, handle, target)?;
        let Some(entry) = block_containing(self.index_scratch, count, target, handle.packed) else {
            return Ok(None);
        };
        let block_ref = block_ref_at(self.index_scratch, entry, handle.packed);
        let data_len = self.load_data_block(store, block_ref)?;
        for entry in (DataBlock {
            bytes: &self.decoded_scratch[..data_len],
        }) {
            if entry.key.rowid == rowid && entry.key.commit_lsn <= snapshot {
                return Ok(Some(SstProbe {
                    key: entry.key,
                    len: (!entry.tombstone).then_some(entry.total_len as u32),
                }));
            }
            if entry.key.rowid > rowid {
                break;
            }
        }
        Ok(None)
    }

    /// Streams every row whose key is in `[lo, hi]`, in key order, to `emit`.
    /// Locates the first covering data block through the sparse index, then
    /// reads consecutive data blocks and emits their in-range rows until one
    /// runs past `hi`. So a range scan fetches the index plus only the data
    /// blocks the range actually covers, not the whole SST.
    #[cfg(test)]
    pub(crate) fn scan(
        &mut self,
        store: &mut dyn BlockStore,
        handle: &SstHandle,
        lo: u64,
        hi: u64,
        emit: &mut dyn FnMut(u64, Option<&[u8]>),
    ) -> Result<(), SstError> {
        if lo > hi {
            return Ok(());
        }
        // A range is not a point-membership question, so the filter does not
        // help here; the index locates the covering blocks directly. Leaves
        // walk in order (a single-block index is its own only leaf), the
        // root refetched per leaf advance through the cache.
        let start_key = SstKey::newest(lo);
        let start_leaf = self.leaf_for(store, handle, start_key)?;
        let mut leaf_ordinal = start_leaf;
        let mut emitted_rowid: Option<u64> = None;
        'leaves: loop {
            let Some(count) = self.load_leaf(store, handle, leaf_ordinal)? else {
                break 'leaves;
            };
            // The block `lo` falls in, or — when `lo` precedes every key — the
            // first block, since the range may still cover it from the left.
            let start = if leaf_ordinal == start_leaf {
                block_containing(self.index_scratch, count, start_key, handle.packed).unwrap_or(0)
            } else {
                0
            };
            for entry_index in start..count {
                let block_ref = block_ref_at(self.index_scratch, entry_index, handle.packed);
                prefetch_data_window(
                    store,
                    self.index_scratch,
                    entry_index + 1,
                    count,
                    handle.packed,
                )?;
                let data_len = self.load_data_block(store, block_ref)?;
                let mut ran_past = false;
                // A chained entry owns its whole block, so at most one assembly
                // happens per block and the borrow of `data_scratch` has ended by
                // the time the chain's overflow blocks are read.
                let mut chained: Option<(u64, usize)> = None;
                for entry in (DataBlock {
                    bytes: &self.decoded_scratch[..data_len],
                }) {
                    if entry.key.rowid > hi {
                        ran_past = true;
                        break;
                    }
                    if entry.key.rowid >= lo && emitted_rowid != Some(entry.key.rowid) {
                        emitted_rowid = Some(entry.key.rowid);
                        if entry.tombstone {
                            emit(entry.key.rowid, None);
                        } else if entry.is_chained() {
                            assemble_chain(store, &entry, self.assembly)?;
                            chained = Some((entry.key.rowid, entry.total_len));
                            break;
                        } else {
                            emit(entry.key.rowid, Some(entry.head));
                        }
                    }
                }
                if let Some((key, n)) = chained {
                    emit(key, Some(&self.assembly[..n]));
                }
                // A block ending past `hi` bounds the scan: later blocks hold only
                // larger keys, so none of them can be in range.
                if ran_past {
                    break 'leaves;
                }
            }
            leaf_ordinal += 1;
        }
        Ok(())
    }

    /// Bounded physical-version scan used by compaction. Unlike `scan`, it
    /// does not collapse versions of one row.
    pub(crate) fn scan_versions_bounded(
        &mut self,
        store: &mut dyn BlockStore,
        handle: &SstHandle,
        lo: SstKey,
        max_blocks: usize,
        emit: &mut dyn FnMut(SstKey, bool),
    ) -> Result<Option<SstKey>, SstError> {
        let start_leaf = self.leaf_for(store, handle, lo)?;
        let mut leaf_ordinal = start_leaf;
        let mut budget = max_blocks;
        let mut last_key: Option<SstKey> = None;
        loop {
            let Some(count) = self.load_leaf(store, handle, leaf_ordinal)? else {
                return Ok(None); // the SST is exhausted
            };
            let start = if leaf_ordinal == start_leaf {
                block_containing(self.index_scratch, count, lo, handle.packed).unwrap_or(0)
            } else {
                0
            };
            let end = (start + budget).min(count);
            for entry_index in start..end {
                let block_ref = block_ref_at(self.index_scratch, entry_index, handle.packed);
                prefetch_data_window(
                    store,
                    self.index_scratch,
                    entry_index + 1,
                    end,
                    handle.packed,
                )?;
                let data_len = self.load_data_block(store, block_ref)?;
                for entry in (DataBlock {
                    bytes: &self.decoded_scratch[..data_len],
                }) {
                    if entry.key >= lo {
                        emit(entry.key, entry.tombstone);
                    }
                    last_key = Some(last_key.map_or(entry.key, |key| key.max(entry.key)));
                }
            }
            budget -= end - start;
            if budget == 0 && end < count {
                break;
            }
            if end == count && budget > 0 {
                leaf_ordinal += 1;
                continue;
            }
            if budget == 0 {
                // The budget ran out exactly at a leaf boundary; whether more
                // leaves follow is the resume's question, not this beat's.
                break;
            }
        }
        Ok(last_key.and_then(SstKey::successor))
    }

    /// Loads leaf `ordinal` of the index into the index scratch and returns
    /// its entry count — `None` past the last leaf. A single-block index is
    /// its own leaf 0; a two-level one descends through the root.
    fn load_leaf(
        &mut self,
        store: &mut dyn BlockStore,
        handle: &SstHandle,
        ordinal: usize,
    ) -> Result<Option<usize>, SstError> {
        load_index(store, &handle.index, self.index_scratch)?;
        let head = u32::from_le_bytes(self.index_scratch[0..4].try_into().unwrap());
        if head != INDEX_ROOT_MAGIC {
            return Ok((ordinal == 0).then_some(head as usize));
        }
        let leaves = u32::from_le_bytes(self.index_scratch[4..8].try_into().unwrap()) as usize;
        if ordinal >= leaves {
            return Ok(None);
        }
        let entry_size = VERSIONED_ROOT_ENTRY;
        let key_bytes = 16;
        let at = 8 + ordinal * entry_size;
        let mut id = [0u8; 32];
        id.copy_from_slice(&self.index_scratch[at + key_bytes + 4..at + entry_size]);
        load_index(store, &BlockId(id), self.index_scratch)?;
        Ok(Some(
            u32::from_le_bytes(self.index_scratch[0..4].try_into().unwrap()) as usize,
        ))
    }

    /// The leaf ordinal whose key range covers `rowid` (the last leaf whose
    /// first key does not exceed it; 0 when `rowid` precedes everything).
    fn leaf_for(
        &mut self,
        store: &mut dyn BlockStore,
        handle: &SstHandle,
        key: SstKey,
    ) -> Result<usize, SstError> {
        load_index(store, &handle.index, self.index_scratch)?;
        let head = u32::from_le_bytes(self.index_scratch[0..4].try_into().unwrap());
        if head != INDEX_ROOT_MAGIC {
            return Ok(0);
        }
        let leaves = u32::from_le_bytes(self.index_scratch[4..8].try_into().unwrap()) as usize;
        Ok(root_leaf_containing(self.index_scratch, leaves, key).unwrap_or(0))
    }

    /// Loads the leaf covering `rowid` and returns its entry count — after
    /// this the classic `block_containing`/`block_id_at` searches apply to
    /// the scratch exactly as with a single-block index.
    fn load_covering_leaf(
        &mut self,
        store: &mut dyn BlockStore,
        handle: &SstHandle,
        key: SstKey,
    ) -> Result<usize, SstError> {
        load_index(store, &handle.index, self.index_scratch)?;
        let head = u32::from_le_bytes(self.index_scratch[0..4].try_into().unwrap());
        if head != INDEX_ROOT_MAGIC {
            // The single-block shape: the root is its own covering leaf, and
            // a point lookup still costs filter + index + one data block.
            return Ok(head as usize);
        }
        let leaves = u32::from_le_bytes(self.index_scratch[4..8].try_into().unwrap()) as usize;
        let leaf = root_leaf_containing(self.index_scratch, leaves, key).unwrap_or(0);
        let entry_size = VERSIONED_ROOT_ENTRY;
        let key_bytes = 16;
        let at = 8 + leaf * entry_size;
        let mut id = [0u8; 32];
        id.copy_from_slice(&self.index_scratch[at + key_bytes + 4..at + entry_size]);
        load_index(store, &BlockId(id), self.index_scratch)?;
        Ok(u32::from_le_bytes(self.index_scratch[0..4].try_into().unwrap()) as usize)
    }
}

/// The root entry (last one whose first key does not exceed `key`) in a
/// fetched two-level root. `None` when `key` precedes every leaf.
fn root_leaf_containing(root: &[u8], leaves: usize, key: SstKey) -> Option<usize> {
    let entry_size = VERSIONED_ROOT_ENTRY;
    let first_key = |i: usize| {
        let at = 8 + i * entry_size;
        read_key(&root[at..])
    };
    if leaves == 0 || key < first_key(0) {
        return None;
    }
    let (mut lo, mut hi) = (0usize, leaves - 1);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if first_key(mid) <= key {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    if first_key(lo).rowid != key.rowid
        && lo + 1 < leaves
        && first_key(lo + 1).rowid == key.rowid
        && key < first_key(lo + 1)
    {
        Some(lo + 1)
    } else {
        Some(lo)
    }
}

/// Resolves global data-block `ordinal` through an SST's index root of
/// either shape, fetching blocks through `store` into `buf` (clobbered).
/// `None` past the end. This is how the storage overlay's member cursors
/// walk an SST without holding its whole index resident.
#[cfg(test)]
pub(crate) fn locate_data_block(
    store: &mut dyn BlockStore,
    handle: &SstHandle,
    buf: &mut [u8],
    ordinal: usize,
) -> Result<Option<BlockId>, SstError> {
    Ok(locate_data_block_with_next(store, handle, buf, ordinal)?
        .map(|(reference, _)| reference.id()))
}

pub(crate) fn locate_data_block_ref(
    store: &mut dyn BlockStore,
    handle: &SstHandle,
    buf: &mut [u8],
    ordinal: usize,
) -> Result<Option<DataBlockRef>, SstError> {
    Ok(locate_data_block_with_next(store, handle, buf, ordinal)?.map(|(reference, _)| reference))
}

/// Resolves one data-block ordinal and, when it shares an index leaf with a
/// successor, returns that successor for scan lookahead. The index scratch is
/// otherwise identical to [`locate_data_block`]'s and remains caller-owned.
pub(crate) fn locate_data_block_with_next(
    store: &mut dyn BlockStore,
    handle: &SstHandle,
    buf: &mut [u8],
    ordinal: usize,
) -> Result<Option<(DataBlockRef, Option<DataBlockLookahead>)>, SstError> {
    load_index(store, &handle.index, buf)?;
    let head = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    if head != INDEX_ROOT_MAGIC {
        let count = head as usize;
        if ordinal >= count {
            return Ok(None);
        }
        let next = (ordinal + 1 < count)
            .then(|| DataBlockLookahead::Data(block_ref_at(buf, ordinal + 1, handle.packed)));
        return Ok(Some((block_ref_at(buf, ordinal, handle.packed), next)));
    }
    let leaves = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
    let entry_size = VERSIONED_ROOT_ENTRY;
    let key_bytes = 16;
    let mut remaining = ordinal;
    for leaf in 0..leaves {
        let at = 8 + leaf * entry_size;
        let count = u32::from_le_bytes(buf[at + key_bytes..at + key_bytes + 4].try_into().unwrap())
            as usize;
        if remaining < count {
            let next_leaf = (leaf + 1 < leaves).then(|| {
                let next_at = 8 + (leaf + 1) * entry_size;
                let mut id = [0u8; 32];
                id.copy_from_slice(&buf[next_at + key_bytes + 4..next_at + entry_size]);
                BlockId(id)
            });
            let mut id = [0u8; 32];
            id.copy_from_slice(&buf[at + key_bytes + 4..at + entry_size]);
            load_index(store, &BlockId(id), buf)?;
            let next = if remaining + 1 < count {
                Some(DataBlockLookahead::Data(block_ref_at(
                    buf,
                    remaining + 1,
                    handle.packed,
                )))
            } else {
                next_leaf.map(DataBlockLookahead::Leaf)
            };
            return Ok(Some((block_ref_at(buf, remaining, handle.packed), next)));
        }
        remaining -= count;
    }
    Ok(None)
}

/// Total data blocks under an SST's index root of either shape.
pub(crate) fn data_block_total(
    store: &mut dyn BlockStore,
    handle: &SstHandle,
    buf: &mut [u8],
) -> Result<usize, SstError> {
    load_index(store, &handle.index, buf)?;
    let head = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    if head != INDEX_ROOT_MAGIC {
        return Ok(head as usize);
    }
    let leaves = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
    let entry_size = VERSIONED_ROOT_ENTRY;
    let key_bytes = 16;
    let mut total = 0usize;
    for leaf in 0..leaves {
        let at = 8 + leaf * entry_size;
        total += u32::from_le_bytes(buf[at + key_bytes..at + key_bytes + 4].try_into().unwrap())
            as usize;
    }
    Ok(total)
}

/// Binary-searches the sparse index for the last block whose first key does not
/// exceed `key` — the only data block `key` can be in. `None` when the index is
/// empty or `key` precedes every block's first key.
fn block_containing(index: &[u8], count: usize, key: SstKey, packed: bool) -> Option<usize> {
    let entry_size = index_entry_size(packed);
    let first_key = |i: usize| {
        let at = 4 + i * entry_size;
        read_key(&index[at..])
    };
    if count == 0 {
        return None;
    }
    if key < first_key(0) {
        // A snapshot target newer than this SST's first version still belongs
        // in the first block when the rowid matches.
        return (key.rowid == first_key(0).rowid).then_some(0);
    }
    let (mut lo, mut hi) = (0usize, count - 1);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if first_key(mid) <= key {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    if first_key(lo).rowid != key.rowid
        && lo + 1 < count
        && first_key(lo + 1).rowid == key.rowid
        && key < first_key(lo + 1)
    {
        Some(lo + 1)
    } else {
        Some(lo)
    }
}

/// The block identity stored in index entry `i`.
fn block_ref_at(index: &[u8], i: usize, packed: bool) -> DataBlockRef {
    let entry_size = index_entry_size(packed);
    let at = 4 + i * entry_size;
    read_data_ref(&index[at + 16..at + entry_size], packed)
}

fn index_entry_size(packed: bool) -> usize {
    if packed {
        PACKED_VERSIONED_INDEX_ENTRY
    } else {
        VERSIONED_INDEX_ENTRY
    }
}

fn write_data_ref(reference: DataBlockRef, into: &mut [u8], packed: bool) {
    if !packed {
        into[..32].copy_from_slice(&reference.id().0);
        return;
    }
    match reference {
        DataBlockRef::Direct(id) => {
            into[..32].fill(0);
            into[32..40].fill(0);
            into[40..72].copy_from_slice(&id.0);
        }
        DataBlockRef::Packed {
            container,
            offset,
            length,
            id,
        } => {
            into[..32].copy_from_slice(&container.0);
            into[32..36].copy_from_slice(&offset.to_le_bytes());
            into[36..40].copy_from_slice(&length.to_le_bytes());
            into[40..72].copy_from_slice(&id.0);
        }
    }
}

fn read_data_ref(bytes: &[u8], packed: bool) -> DataBlockRef {
    if !packed {
        let mut id = [0u8; 32];
        id.copy_from_slice(&bytes[..32]);
        return DataBlockRef::Direct(BlockId(id));
    }
    let mut container = [0u8; 32];
    container.copy_from_slice(&bytes[..32]);
    let mut id = [0u8; 32];
    id.copy_from_slice(&bytes[40..72]);
    if container == [0; 32] {
        DataBlockRef::Direct(BlockId(id))
    } else {
        DataBlockRef::Packed {
            container: BlockId(container),
            offset: u32::from_le_bytes(bytes[32..36].try_into().expect("packed offset")),
            length: u32::from_le_bytes(bytes[36..40].try_into().expect("packed length")),
            id: BlockId(id),
        }
    }
}

fn write_key(key: SstKey, into: &mut [u8]) {
    into[0..8].copy_from_slice(&key.rowid.to_le_bytes());
    into[8..16].copy_from_slice(&key.commit_lsn.to_le_bytes());
}

fn read_key(bytes: &[u8]) -> SstKey {
    let rowid = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let commit_lsn = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    SstKey::at(rowid, commit_lsn)
}

fn load_index(
    store: &mut dyn BlockStore,
    id: &BlockId,
    into: &mut [u8],
) -> Result<usize, SstError> {
    let (length, block_type) = store.get(id, into)?;
    validate_index_type(block_type)?;
    Ok(length)
}

fn validate_index_type(block_type: BlockType) -> Result<(), SstError> {
    if block_type != BlockType::SstIndexV2 {
        return Err(SstError::Store(StoreError::Corrupt(
            super::BlockError::UnknownType,
        )));
    }
    Ok(())
}

/// One row read out of a data block. For an ordinary entry `head` is the
/// whole row and `chain` is empty; a chained entry's `head` is the leading
/// chunk and `chain` the overflow blocks' identities (32 bytes each), with
/// `total_len` the assembled row's length.
struct DataEntry<'a> {
    key: SstKey,
    total_len: usize,
    head: &'a [u8],
    chain: &'a [u8],
    tombstone: bool,
}

impl DataEntry<'_> {
    fn is_chained(&self) -> bool {
        !self.chain.is_empty()
    }
}

/// Iterates the `(key, len, row)` entries packed in a data block, in the key
/// order they were written. A short trailing fragment — never present in a
/// well-formed block — ends iteration rather than reading past the payload.
struct DataBlock<'a> {
    bytes: &'a [u8],
}

impl<'a> Iterator for DataBlock<'a> {
    type Item = DataEntry<'a>;

    fn next(&mut self) -> Option<DataEntry<'a>> {
        let data = self.bytes;
        let header = VERSIONED_ENTRY_HEADER;
        if data.len() < header {
            return None;
        }
        let rowid = u64::from_le_bytes(data[0..8].try_into().unwrap());
        let commit_lsn = u64::from_le_bytes(data[8..16].try_into().unwrap());
        let key = SstKey::at(rowid, commit_lsn);
        let raw_len = u32::from_le_bytes(data[16..20].try_into().unwrap());
        if raw_len & TOMB_FLAG != 0 {
            self.bytes = &data[header..];
            return Some(DataEntry {
                key,
                total_len: 0,
                head: &[],
                chain: &[],
                tombstone: true,
            });
        }
        if raw_len & CHAIN_FLAG != 0 {
            // A chained head fills the rest of its block: count, identities,
            // then the leading chunk.
            let total_len = (raw_len & !CHAIN_FLAG) as usize;
            let body = &data[header..];
            if body.len() < 2 {
                return None;
            }
            let n_chunks = u16::from_le_bytes(body[0..2].try_into().unwrap()) as usize;
            if n_chunks > MAX_CHAIN || body.len() < 2 + n_chunks * 32 {
                return None;
            }
            let chain = &body[2..2 + n_chunks * 32];
            let head = &body[2 + n_chunks * 32..];
            self.bytes = &[];
            return Some(DataEntry {
                key,
                total_len,
                head,
                chain,
                tombstone: false,
            });
        }
        let len = raw_len as usize;
        if data.len() < header + len {
            return None;
        }
        self.bytes = &data[header + len..];
        Some(DataEntry {
            key,
            total_len: len,
            head: &data[header..header + len],
            chain: &[],
            tombstone: false,
        })
    }
}

/// One `(rowid, tombstone, total_len, next_offset)` step through a data
/// block's entries without copying row bytes — how the row-map overlay's
/// merged enumeration walks keys. `at` is the previous step's returned
/// offset (0 to start); `None` is the block's end.
pub(crate) fn block_keys_at(block: &[u8], at: usize) -> Option<(SstKey, bool, u32, usize)> {
    if at >= block.len() {
        return None;
    }
    let remaining = &block[at..];
    let before = remaining.len();
    let mut entries = DataBlock { bytes: remaining };
    let entry = entries.next()?;
    let consumed = before - entries.bytes.len();
    Some((
        entry.key,
        entry.tombstone,
        entry.total_len as u32,
        at + consumed,
    ))
}

/// Copies the entry beginning at `at` out of a resident data block.  A merged
/// table walk has already paid to fetch this block; copying its selected row
/// here keeps the scan from issuing a second point read for that same row.
/// The returned key and tombstone flag let the caller retain the ordinary
/// merged-version checks at its visibility boundary.
pub(crate) fn copy_block_entry_at(
    store: &mut dyn BlockStore,
    block: &[u8],
    at: usize,
    out: &mut [u8],
) -> Result<(SstKey, bool, usize), SstError> {
    let remaining = block.get(at..).ok_or(SstError::Store(StoreError::Corrupt(
        super::BlockError::Truncated,
    )))?;
    let mut entries = DataBlock { bytes: remaining };
    let entry = entries.next().ok_or(SstError::Store(StoreError::Corrupt(
        super::BlockError::Truncated,
    )))?;
    if entry.tombstone {
        return Ok((entry.key, true, 0));
    }
    if out.len() != entry.total_len {
        return Err(SstError::Store(StoreError::BufferTooSmall));
    }
    if entry.is_chained() {
        assemble_chain(store, &entry, out)?;
    } else {
        out.copy_from_slice(entry.head);
    }
    Ok((entry.key, false, entry.total_len))
}

/// Copies a chained entry's row into `into`: the inline head chunk, then each
/// overflow block in order. `into` must hold `total_len` bytes.
fn assemble_chain(
    store: &mut dyn BlockStore,
    entry: &DataEntry<'_>,
    into: &mut [u8],
) -> Result<(), SstError> {
    if into.len() < entry.total_len {
        return Err(SstError::Store(StoreError::BufferTooSmall));
    }
    into[..entry.head.len()].copy_from_slice(entry.head);
    let mut at = entry.head.len();
    for id_bytes in entry.chain.chunks(32) {
        let mut id = [0u8; 32];
        id.copy_from_slice(id_bytes);
        let (n, _) = store.get(&BlockId(id), &mut into[at..])?;
        at += n;
    }
    if at != entry.total_len {
        return Err(SstError::Store(StoreError::Corrupt(
            super::BlockError::Truncated,
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;
    use crate::sql::types::Datum;
    use crate::storage::rowenc;
    use crate::store::memory::MemoryBlockStore;

    fn store() -> (Budget, MemoryBlockStore) {
        let mut budget = Budget::new(64 << 20);
        let s = MemoryBlockStore::new(&mut budget, "sst store", 16 << 20, 4096).expect("fits");
        (budget, s)
    }

    fn arena() -> Arena {
        let mut budget = Budget::new(64 << 20);
        Arena::new(&mut budget, "sst test arena", 32 << 20).expect("arena fits")
    }

    /// Builds an SST from `(rowid, row)` pairs, returns its root.
    fn build(store: &mut MemoryBlockStore, rows: &[(u64, Vec<u8>)]) -> Option<SstHandle> {
        let mut w = SstWriter::new();
        for (rowid, row) in rows {
            w.append(store, *rowid, row).unwrap();
        }
        w.finish(store).unwrap()
    }

    fn get(
        reader: &mut SstReader,
        store: &mut MemoryBlockStore,
        handle: &SstHandle,
        rowid: u64,
    ) -> Option<Vec<u8>> {
        let mut out = vec![0u8; MAX_PAYLOAD];
        reader
            .get(store, handle, rowid, &mut out)
            .unwrap()
            .map(|n| {
                out.truncate(n);
                out
            })
    }

    #[test]
    fn one_row_round_trips() {
        let (_b, mut s) = store();
        let a = arena();
        let root = build(&mut s, &[(1, b"only row".to_vec())]).expect("has a root");
        let mut r = SstReader::new(&a).unwrap();
        assert_eq!(
            get(&mut r, &mut s, &root, 1).as_deref(),
            Some(&b"only row"[..])
        );
        assert_eq!(get(&mut r, &mut s, &root, 2), None);
        assert_eq!(get(&mut r, &mut s, &root, 0), None);
    }

    #[test]
    fn pax_rows_and_tombstones_reassemble_as_canonical_rows() {
        let (_budget, mut store) = store();
        let arena = arena();
        let schema = [ColType::Int4, ColType::Text, ColType::Bool];
        let first = [Datum::Int4(7), Datum::Text("wide value"), Datum::Null];
        let second = [Datum::Int4(-4), Datum::Text("second"), Datum::Bool(true)];
        let mut first_bytes = vec![0; rowenc::encoded_len(&first)];
        let mut second_bytes = vec![0; rowenc::encoded_len(&second)];
        rowenc::encode(&first, &mut first_bytes);
        rowenc::encode(&second, &mut second_bytes);
        let mut writer = SstWriter::new();
        writer.set_pax_schema(&schema).unwrap();
        writer
            .append_version(&mut store, SstKey::at(1, 20), &first_bytes)
            .unwrap();
        writer
            .append_tombstone_version(&mut store, SstKey::at(2, 20))
            .unwrap();
        writer
            .append_version(&mut store, SstKey::at(3, 20), &second_bytes)
            .unwrap();
        let handle = writer.finish(&mut store).unwrap().unwrap();
        let mut reader = SstReader::new(&arena).unwrap();
        assert_eq!(get(&mut reader, &mut store, &handle, 1), Some(first_bytes));
        assert_eq!(get(&mut reader, &mut store, &handle, 2), None);
        let before_second_live_row = store.reads();
        assert_eq!(get(&mut reader, &mut store, &handle, 3), Some(second_bytes));
        assert_eq!(
            store.reads() - before_second_live_row,
            2,
            "the filter and index are needed again, but the decoded PAX descriptor is reused"
        );
    }

    #[test]
    fn reset_returns_a_pax_writer_to_canonical_rows() {
        let (_budget, mut store) = store();
        let mut writer = SstWriter::new();
        writer.set_pax_schema(&[ColType::Int4]).unwrap();
        writer.reset();
        writer
            .append_version(&mut store, SstKey::at(1, 1), b"ordinary external row")
            .unwrap();
        let handle = writer.finish(&mut store).unwrap().unwrap();
        let mut index = [0; MAX_PAYLOAD];
        let reference = locate_data_block_with_next(&mut store, &handle, &mut index, 0)
            .unwrap()
            .unwrap()
            .0;
        let mut data = [0; MAX_PAYLOAD];
        let mut scratch = [0; MAX_PAYLOAD];
        let (_, block_type) =
            read_data_block_raw_ref(&mut store, reference, &mut data, &mut scratch).unwrap();
        assert!(matches!(
            block_type,
            BlockType::SstDataV2 | BlockType::SstDataV2Lz4
        ));
    }

    #[test]
    fn pax_column_extents_rebuild_only_the_selected_row() {
        let (_budget, mut store) = store();
        let schema = [ColType::Int4, ColType::Text, ColType::Bool];
        let first = [Datum::Int4(7), Datum::Text("wide value"), Datum::Null];
        let second = [Datum::Int4(-4), Datum::Text("second"), Datum::Bool(true)];
        let mut first_bytes = vec![0; rowenc::encoded_len(&first)];
        let mut second_bytes = vec![0; rowenc::encoded_len(&second)];
        rowenc::encode(&first, &mut first_bytes);
        rowenc::encode(&second, &mut second_bytes);
        let mut writer = SstWriter::new();
        writer.set_pax_schema(&schema).unwrap();
        writer
            .append_version(&mut store, SstKey::at(1, 20), &first_bytes)
            .unwrap();
        writer
            .append_tombstone_version(&mut store, SstKey::at(2, 20))
            .unwrap();
        writer
            .append_version(&mut store, SstKey::at(3, 20), &second_bytes)
            .unwrap();
        let handle = writer.finish(&mut store).unwrap().unwrap();
        let mut index = [0; MAX_PAYLOAD];
        let reference = locate_data_block_with_next(&mut store, &handle, &mut index, 0)
            .unwrap()
            .unwrap()
            .0;
        let mut raw = [0; MAX_PAYLOAD];
        let mut scratch = [0; MAX_PAYLOAD];
        let (length, kind) =
            read_data_block_raw_ref(&mut store, reference, &mut raw, &mut scratch).unwrap();
        assert_eq!(kind, BlockType::SstDataPaxV2);
        let layout = pax_layout(&raw[..length]).unwrap();
        let mut oversized_row = raw[..length].to_vec();
        let row_length = 8 + layout.columns() + PAX_ROW_HEADER;
        oversized_row[row_length..row_length + 4]
            .copy_from_slice(&((MAX_INLINE_ROW + 1) as u32).to_le_bytes());
        assert!(
            pax_layout(&oversized_row).is_err(),
            "a PAX descriptor must reject a row length beyond the fixed row buffer"
        );
        assert_ne!(
            layout.column_ref(&raw[..length], 0).unwrap(),
            layout.column_ref(&raw[..length], 1).unwrap()
        );
        let mut output = [0; MAX_PAYLOAD];
        let copied = copy_pax_v2_row_demand(
            &mut store,
            &layout,
            &raw[..length],
            0,
            None,
            &mut PaxReadScratch {
                column: &mut scratch,
                range: &mut index,
            },
            &mut output,
        )
        .unwrap();
        assert_eq!(&output[..copied], first_bytes);
        let copied = copy_pax_v2_row_demand(
            &mut store,
            &layout,
            &raw[..length],
            2,
            None,
            &mut PaxReadScratch {
                column: &mut scratch,
                range: &mut index,
            },
            &mut output,
        )
        .unwrap();
        assert_eq!(&output[..copied], second_bytes);
    }

    #[test]
    fn pax_columns_share_one_verified_packed_container() {
        let (_budget, mut store) = store();
        let schema = [ColType::Int4, ColType::Text];
        let payload = "x".repeat(20_000);
        let mut writer = SstWriter::new();
        writer.set_pax_schema(&schema).unwrap();
        for rowid in 1..=8 {
            let row = [Datum::Int4(rowid), Datum::Text(&payload)];
            let mut encoded = vec![0; rowenc::encoded_len(&row)];
            rowenc::encode(&row, &mut encoded);
            writer
                .append_version(&mut store, SstKey::at(rowid as u64, 1), &encoded)
                .unwrap();
        }
        let handle = writer.finish(&mut store).unwrap().unwrap();
        assert!(handle.packed);

        let mut index = [0; MAX_PAYLOAD];
        let first = locate_data_block_ref(&mut store, &handle, &mut index, 0)
            .unwrap()
            .unwrap();
        let mut raw = [0; MAX_PAYLOAD];
        let mut scratch = [0; MAX_PAYLOAD];
        let (length, kind) =
            read_data_block_raw_ref(&mut store, first, &mut raw, &mut scratch).unwrap();
        assert_eq!(kind, BlockType::SstDataPaxV2);
        let layout = pax_layout(&raw[..length]).unwrap();
        assert_eq!(layout.rows(), 6);
        let (first_container, first_offset) = match layout.column_ref(&raw[..length], 0).unwrap() {
            DataBlockRef::Packed {
                container, offset, ..
            } => (container, offset),
            other => panic!("expected packed PAX column reference, got {other:?}"),
        };
        let (second_container, second_offset) = match layout.column_ref(&raw[..length], 1).unwrap()
        {
            DataBlockRef::Packed {
                container, offset, ..
            } => (container, offset),
            other => panic!("expected packed PAX column reference, got {other:?}"),
        };
        assert_eq!(first_container, second_container);
        assert_ne!(first_offset, second_offset);
    }

    #[test]
    fn an_empty_sst_has_no_root() {
        let (_b, mut s) = store();
        assert_eq!(build(&mut s, &[]), None);
    }

    #[test]
    fn every_row_is_found_across_many_data_blocks() {
        // Rows large enough that thousands span many data blocks, so the sparse
        // index and its binary search are actually exercised rather than a
        // single-block SST that never consults the index arithmetic.
        let (_b, mut s) = store();
        let a = arena();
        let rows: Vec<_> = (0..5000u64)
            .map(|i| (i * 2 + 1, vec![i as u8; 400]))
            .collect();
        let root = build(&mut s, &rows).expect("has a root");
        let mut r = SstReader::new(&a).unwrap();
        for (rowid, row) in &rows {
            assert_eq!(
                get(&mut r, &mut s, &root, *rowid).as_ref(),
                Some(row),
                "row {rowid}"
            );
        }
        // Every gap between the odd keys is absent, and the ends too.
        assert_eq!(get(&mut r, &mut s, &root, 0), None);
        assert_eq!(get(&mut r, &mut s, &root, 2), None);
        assert_eq!(get(&mut r, &mut s, &root, 10_001), None);
    }

    #[test]
    fn a_present_key_reads_the_filter_the_index_and_one_data_block() {
        // The sparse index's guarantee, now with the filter in front: whatever
        // the SST's size, a hit costs the filter, the index, and one data block.
        let (_b, mut s) = store();
        let a = arena();
        let rows: Vec<_> = (0..3000u64).map(|i| (i + 1, vec![7u8; 500])).collect();
        let root = build(&mut s, &rows).expect("has a root");
        let mut r = SstReader::new(&a).unwrap();
        let before = s.reads();
        let _ = get(&mut r, &mut s, &root, 2500);
        assert_eq!(s.reads() - before, 3, "filter, index, and one data block");
    }

    #[test]
    fn a_filtered_out_key_reads_only_the_filter() {
        // The filter's payoff: a key it rejects returns without the index or a
        // data block being touched at all — one read, not three.
        let (_b, mut s) = store();
        let a = arena();
        // Only even keys are stored, so the odd probe is genuinely absent; with
        // 3000 keys in a 128 KiB filter a false positive is very unlikely, and
        // a rare one would read more, so the test probes several odd keys and
        // requires that most cost a single read.
        let rows: Vec<_> = (0..3000u64).map(|i| (i * 2, vec![7u8; 500])).collect();
        let root = build(&mut s, &rows).expect("has a root");
        let mut r = SstReader::new(&a).unwrap();
        let mut single_read = 0;
        for probe in (1..200u64).step_by(2) {
            let before = s.reads();
            assert_eq!(
                get(&mut r, &mut s, &root, probe),
                None,
                "odd key {probe} is absent"
            );
            if s.reads() - before == 1 {
                single_read += 1;
            }
        }
        assert!(
            single_read >= 95,
            "the filter skipped the index on {single_read} of 100 absent keys"
        );
    }

    #[test]
    fn rows_out_of_order_are_refused() {
        let (_b, mut s) = store();
        let mut w = SstWriter::new();
        w.append(&mut s, 5, b"five").unwrap();
        assert_eq!(
            w.append(&mut s, 3, b"three").err(),
            Some(SstError::KeyOutOfOrder)
        );
        assert_eq!(
            w.append(&mut s, 5, b"again").err(),
            Some(SstError::KeyOutOfOrder)
        );
    }

    #[test]
    fn a_row_larger_than_a_block_chains_and_round_trips() {
        // A row past one block's payload spans overflow blocks and reads back
        // byte-identical, by point lookup and by scan, with ordinary rows on
        // both sides of it.
        let (_b, mut s) = store();
        let a = Arena::new(&mut Budget::new(64 << 20), "sst chain", 16 << 20).expect("arena");
        let mut w = SstWriter::new();
        let huge: Vec<u8> = (0..MAX_PAYLOAD + 50_000)
            .map(|i| (i * 31 % 251) as u8)
            .collect();
        w.append(&mut s, 1, &[7u8; 40]).unwrap();
        w.append(&mut s, 2, &huge).unwrap();
        w.append(&mut s, 3, &[8u8; 40]).unwrap();
        let root = w.finish(&mut s).unwrap().expect("root");
        let mut r = SstReader::new(&a).unwrap();
        let mut out = vec![0u8; MAX_ASSEMBLED];
        let n = r.get(&mut s, &root, 2, &mut out).unwrap().expect("found");
        assert_eq!(&out[..n], &huge[..], "chained row round-trips by get");
        let mut seen = Vec::new();
        r.scan(&mut s, &root, 0, u64::MAX, &mut |k, row| {
            seen.push((k, row.expect("data row").to_vec()))
        })
        .unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[1].0, 2);
        assert_eq!(seen[1].1, huge, "chained row round-trips by scan");
        assert_eq!(seen[0].1, vec![7u8; 40]);
        assert_eq!(seen[2].1, vec![8u8; 40]);
    }

    #[test]
    fn a_tombstone_round_trips_and_hides_the_key() {
        // A delta SST records deletions as tombstones: a scan reports them
        // (None), a point lookup treats the key as absent, and ordering with
        // ordinary rows holds.
        let (_b, mut s) = store();
        let a = arena();
        let mut w = SstWriter::new();
        w.append(&mut s, 1, &[7u8; 8]).unwrap();
        w.append_tombstone(&mut s, 2).unwrap();
        w.append(&mut s, 3, &[9u8; 8]).unwrap();
        let root = w.finish(&mut s).unwrap().expect("root");
        let mut r = SstReader::new(&a).unwrap();
        let mut out = [0u8; 64];
        assert_eq!(
            r.get(&mut s, &root, 2, &mut out).unwrap(),
            None,
            "tombstoned key is absent"
        );
        assert!(r.get(&mut s, &root, 1, &mut out).unwrap().is_some());
        let mut seen = Vec::new();
        r.scan(&mut s, &root, 0, u64::MAX, &mut |k, row| {
            seen.push((k, row.is_none()))
        })
        .unwrap();
        assert_eq!(seen, vec![(1, false), (2, true), (3, false)]);
    }

    #[test]
    fn commit_lsn_versions_select_the_postgresql_snapshot_image() {
        let (_budget, mut store) = store();
        let mut writer = SstWriter::new();
        writer
            .append_version(&mut store, SstKey::at(7, 30), b"thirty")
            .unwrap();
        writer
            .append_tombstone_version(&mut store, SstKey::at(7, 20))
            .unwrap();
        writer
            .append_version(&mut store, SstKey::at(7, 10), b"ten")
            .unwrap();
        let handle = writer.finish(&mut store).unwrap().unwrap();
        let arena = arena();
        let mut reader = SstReader::new(&arena).unwrap();
        let mut out = [0u8; 32];

        let newest = reader
            .get_at(&mut store, &handle, 7, 35, &mut out)
            .unwrap()
            .unwrap();
        assert_eq!(newest.key.commit_lsn, 30);
        assert_eq!(&out[..newest.len.unwrap() as usize], b"thirty");
        assert_eq!(
            reader
                .probe_at(&mut store, &handle, 7, 25)
                .unwrap()
                .unwrap(),
            SstProbe {
                key: SstKey::at(7, 20),
                len: None,
            }
        );
        let old = reader
            .get_at(&mut store, &handle, 7, 15, &mut out)
            .unwrap()
            .unwrap();
        assert_eq!(old.key.commit_lsn, 10);
        assert_eq!(&out[..old.len.unwrap() as usize], b"ten");
        assert!(
            reader
                .probe_at(&mut store, &handle, 7, 9)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn one_rows_versions_may_cross_data_block_boundaries() {
        let (_budget, mut store) = store();
        let mut writer = SstWriter::new();
        let row = vec![0x5au8; MAX_PAYLOAD / 2];
        for commit_lsn in (1..=6).rev() {
            writer
                .append_version(&mut store, SstKey::at(9, commit_lsn), &row)
                .unwrap();
        }
        let handle = writer.finish(&mut store).unwrap().unwrap();
        let arena = arena();
        let mut reader = SstReader::new(&arena).unwrap();
        let mut out = vec![0u8; row.len()];
        for snapshot in 1..=6 {
            let probe = reader
                .get_at(&mut store, &handle, 9, snapshot, &mut out)
                .unwrap()
                .unwrap();
            assert_eq!(probe.key.commit_lsn, snapshot);
            assert_eq!(&out[..probe.len.unwrap() as usize], &row);
        }
    }

    #[test]
    fn a_row_beyond_the_chain_bound_is_refused() {
        let (_b, mut s) = store();
        let mut w = SstWriter::new();
        let huge = vec![0u8; MAX_ASSEMBLED + MAX_PAYLOAD];
        assert_eq!(
            w.append(&mut s, 1, &huge).err(),
            Some(SstError::RowTooLarge)
        );
    }

    #[test]
    fn a_short_output_buffer_is_refused() {
        let (_b, mut s) = store();
        let a = arena();
        let root = build(&mut s, &[(1, vec![9u8; 100])]).expect("root");
        let mut r = SstReader::new(&a).unwrap();
        let mut small = [0u8; 10];
        assert_eq!(
            r.get(&mut s, &root, 1, &mut small).err(),
            Some(SstError::Store(StoreError::BufferTooSmall))
        );
    }

    fn scan(
        reader: &mut SstReader,
        store: &mut MemoryBlockStore,
        handle: &SstHandle,
        lo: u64,
        hi: u64,
    ) -> Vec<(u64, Vec<u8>)> {
        let mut out = Vec::new();
        reader
            .scan(store, handle, lo, hi, &mut |key, row| {
                out.push((key, row.expect("data row").to_vec()))
            })
            .unwrap();
        out
    }

    struct LookaheadStore {
        inner: MemoryBlockStore,
        requested: Vec<BlockId>,
        prefetched: Vec<BlockId>,
    }

    impl BlockStore for LookaheadStore {
        fn put(
            &mut self,
            payload: &[u8],
            block_type: BlockType,
            lsn: u64,
        ) -> Result<BlockId, StoreError> {
            self.inner.put(payload, block_type, lsn)
        }

        fn get(&mut self, id: &BlockId, into: &mut [u8]) -> Result<(usize, BlockType), StoreError> {
            self.requested.push(*id);
            self.inner.get(id, into)
        }

        fn contains(&mut self, id: &BlockId) -> Result<bool, StoreError> {
            self.inner.contains(id)
        }

        fn async_gets_enabled(&self) -> bool {
            true
        }

        fn async_read_slots(&self) -> usize {
            4
        }

        fn prefetch(&mut self, id: &BlockId) -> Result<crate::store::PrefetchState, StoreError> {
            self.requested.push(*id);
            self.prefetched.push(*id);
            Ok(crate::store::PrefetchState::Scheduled)
        }

        fn take_prefetch(
            &mut self,
            id: &BlockId,
            into: &mut [u8],
        ) -> Result<Option<(usize, BlockType)>, StoreError> {
            let Some(position) = self.prefetched.iter().position(|candidate| candidate == id)
            else {
                return Ok(None);
            };
            self.prefetched.swap_remove(position);
            self.inner.get(id, into).map(Some)
        }
    }

    #[test]
    fn scan_requests_the_next_data_block_before_the_current_one() {
        let (_budget, mut inner) = store();
        let row = vec![7u8; MAX_PAYLOAD / 2];
        let handle = build(
            &mut inner,
            &[(1, row.clone()), (2, row.clone()), (3, row.clone())],
        )
        .unwrap();
        let mut index = [0u8; MAX_PAYLOAD];
        let first = locate_data_block(&mut inner, &handle, &mut index, 0)
            .unwrap()
            .unwrap();
        let second = locate_data_block(&mut inner, &handle, &mut index, 1)
            .unwrap()
            .unwrap();
        let mut store = LookaheadStore {
            inner,
            requested: Vec::new(),
            prefetched: Vec::new(),
        };
        let arena = arena();
        let mut reader = SstReader::new(&arena).unwrap();
        let mut rows = 0;
        reader
            .scan(&mut store, &handle, 1, 3, &mut |_, _| rows += 1)
            .unwrap();
        assert_eq!(rows, 3);
        let requests: Vec<_> = store
            .requested
            .iter()
            .copied()
            .filter(|id| *id == first || *id == second)
            .collect();
        assert_eq!(requests[..2], [second, first]);
    }

    #[test]
    fn scan_fills_the_bounded_lookahead_window_from_one_index_leaf() {
        let (_budget, mut inner) = store();
        let row = vec![7u8; MAX_PAYLOAD / 2];
        let handle = build(
            &mut inner,
            &[
                (1, row.clone()),
                (2, row.clone()),
                (3, row.clone()),
                (4, row.clone()),
                (5, row.clone()),
            ],
        )
        .unwrap();
        let mut index = [0u8; MAX_PAYLOAD];
        let mut ids = [BlockId([0; 32]); 4];
        for (ordinal, id) in ids.iter_mut().enumerate() {
            *id = locate_data_block(&mut inner, &handle, &mut index, ordinal)
                .unwrap()
                .unwrap();
        }
        let mut store = LookaheadStore {
            inner,
            requested: Vec::new(),
            prefetched: Vec::new(),
        };
        let arena = arena();
        let mut reader = SstReader::new(&arena).unwrap();
        reader
            .scan(&mut store, &handle, 1, 5, &mut |_, _| {})
            .unwrap();
        let requests: Vec<_> = store
            .requested
            .iter()
            .copied()
            .filter(|id| ids.contains(id))
            .collect();
        assert_eq!(requests[..4], [ids[1], ids[2], ids[3], ids[0]]);
    }

    #[test]
    fn a_range_scan_returns_the_covered_rows_in_order() {
        let (_b, mut s) = store();
        let a = arena();
        let rows: Vec<_> = (1..=50u64).map(|i| (i, vec![i as u8; 20])).collect();
        let root = build(&mut s, &rows).expect("root");
        let mut r = SstReader::new(&a).unwrap();
        let got = scan(&mut r, &mut s, &root, 10, 20);
        assert_eq!(got.len(), 11);
        assert_eq!(got.first().unwrap().0, 10);
        assert_eq!(got.last().unwrap().0, 20);
        for (i, (key, row)) in got.iter().enumerate() {
            assert_eq!(*key, 10 + i as u64);
            assert_eq!(row, &vec![*key as u8; 20]);
        }
    }

    #[test]
    fn a_range_spanning_many_data_blocks_is_complete_and_ordered() {
        // Rows big enough to span many blocks, so the scan must walk from the
        // block `lo` lands in through the consecutive blocks the range covers.
        let (_b, mut s) = store();
        let a = arena();
        let rows: Vec<_> = (0..4000u64)
            .map(|i| (i, vec![(i % 251) as u8; 400]))
            .collect();
        let root = build(&mut s, &rows).expect("root");
        let mut r = SstReader::new(&a).unwrap();
        let got = scan(&mut r, &mut s, &root, 1000, 2999);
        assert_eq!(got.len(), 2000);
        for (expected, (key, row)) in (1000u64..).zip(got.iter()) {
            assert_eq!(*key, expected, "keys must be dense and ascending");
            assert_eq!(row, &vec![(expected % 251) as u8; 400]);
        }
    }

    #[test]
    fn range_bounds_beyond_the_data_clamp_to_what_exists() {
        let (_b, mut s) = store();
        let a = arena();
        let rows: Vec<_> = (10..=30u64).map(|i| (i, vec![i as u8; 8])).collect();
        let root = build(&mut s, &rows).expect("root");
        let mut r = SstReader::new(&a).unwrap();
        // Below, above, and straddling both ends.
        assert_eq!(
            scan(&mut r, &mut s, &root, 0, 5).len(),
            0,
            "before the first key"
        );
        assert_eq!(
            scan(&mut r, &mut s, &root, 40, 99).len(),
            0,
            "after the last key"
        );
        assert_eq!(
            scan(&mut r, &mut s, &root, 0, 100).len(),
            21,
            "covers everything"
        );
        let straddle_low = scan(&mut r, &mut s, &root, 5, 12);
        assert_eq!(
            straddle_low.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            vec![10, 11, 12]
        );
        let straddle_high = scan(&mut r, &mut s, &root, 28, 50);
        assert_eq!(
            straddle_high.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            vec![28, 29, 30]
        );
    }

    #[test]
    fn a_single_key_range_returns_just_that_row() {
        let (_b, mut s) = store();
        let a = arena();
        let rows: Vec<_> = (1..=40u64).map(|i| (i * 3, vec![i as u8; 16])).collect();
        let root = build(&mut s, &rows).expect("root");
        let mut r = SstReader::new(&a).unwrap();
        assert_eq!(
            scan(&mut r, &mut s, &root, 30, 30),
            vec![(30, vec![10u8; 16])]
        );
        // A key that falls in a gap between stored keys returns nothing.
        assert_eq!(scan(&mut r, &mut s, &root, 31, 31), vec![]);
    }

    #[test]
    fn an_inverted_range_is_empty() {
        let (_b, mut s) = store();
        let a = arena();
        let rows: Vec<_> = (1..=10u64).map(|i| (i, vec![i as u8; 4])).collect();
        let root = build(&mut s, &rows).expect("root");
        let mut r = SstReader::new(&a).unwrap();
        assert_eq!(
            scan(&mut r, &mut s, &root, 8, 3),
            vec![],
            "hi below lo yields nothing"
        );
    }

    #[test]
    fn a_scan_over_a_range_reads_the_index_plus_only_its_blocks() {
        // The point of streaming the covering blocks: a narrow range near the
        // end of a large SST reads the index and a handful of data blocks, not
        // the whole table.
        let (_b, mut s) = store();
        let a = arena();
        let rows: Vec<_> = (0..3000u64).map(|i| (i, vec![9u8; 500])).collect();
        let root = build(&mut s, &rows).expect("root");
        let mut r = SstReader::new(&a).unwrap();
        let before = s.reads();
        let got = scan(&mut r, &mut s, &root, 2500, 2510);
        assert_eq!(got.len(), 11);
        let read = s.reads() - before;
        // Index + the one or two data blocks an eleven-key window touches.
        assert!(
            read <= 4,
            "a narrow range read {read} blocks; expected the index and a few data blocks"
        );
    }

    #[test]
    fn a_two_level_index_serves_every_read_path() {
        // Forcing leaf flushes between appends builds a real two-level SST
        // without the gigabytes a naturally-overflowing index would need:
        // five leaves of two data blocks each, verified through every
        // navigation path — point get, probe, full scan, bounded scan
        // resumption, and the ordinal resolvers the overlay cursors use.
        let (_b, mut s) = store();
        let a = arena();
        let mut w = SstWriter::new();
        let row = |i: u64| vec![(i % 251) as u8; 100_000]; // ~2 rows per block
        let mut written = Vec::new();
        for group in 0..5u64 {
            for i in 0..4u64 {
                let rowid = group * 100 + i * 3 + 1;
                w.append(&mut s, rowid, &row(rowid)).unwrap();
                written.push(rowid);
            }
            w.flush_data(&mut s).unwrap();
            w.flush_index_leaf(&mut s).unwrap();
        }
        let handle = w.finish(&mut s).unwrap().expect("root");

        let mut r = SstReader::new(&a).unwrap();
        // The root really is two-level.
        let mut buf = vec![0u8; MAX_PAYLOAD];
        let (_, _) = s.get(&handle.index, &mut buf).unwrap();
        assert_eq!(
            u32::from_le_bytes(buf[0..4].try_into().unwrap()),
            INDEX_ROOT_MAGIC
        );
        let total = super::data_block_total(&mut s, &handle, &mut buf).unwrap();
        assert!(total >= 10, "five groups of two blocks, got {total}");
        // Every ordinal resolves; one past the end does not.
        for ordinal in 0..total {
            assert!(
                super::locate_data_block(&mut s, &handle, &mut buf, ordinal)
                    .unwrap()
                    .is_some()
            );
        }
        assert!(
            super::locate_data_block(&mut s, &handle, &mut buf, total)
                .unwrap()
                .is_none()
        );
        // Point reads and probes, present and absent.
        let mut out = vec![0u8; 128 * 1024];
        for &rowid in &written {
            assert_eq!(
                r.get(&mut s, &handle, rowid, &mut out).unwrap(),
                Some(100_000),
                "row {rowid}"
            );
            assert_eq!(
                r.probe(&mut s, &handle, rowid).unwrap(),
                Some(Some(100_000))
            );
        }
        assert_eq!(r.get(&mut s, &handle, 2, &mut out).unwrap(), None);
        assert_eq!(r.probe(&mut s, &handle, 100_000).unwrap(), None);
        // A full scan crosses every leaf in order.
        let mut seen = Vec::new();
        r.scan(&mut s, &handle, 0, u64::MAX, &mut |rowid, row| {
            assert!(row.is_some());
            seen.push(rowid);
        })
        .unwrap();
        assert_eq!(seen, written);
        // A bounded scan resumes across leaf boundaries to the same total.
        let mut walked = Vec::new();
        let mut lo = SstKey::MIN;
        while let Some(next) = r
            .scan_versions_bounded(&mut s, &handle, lo, 3, &mut |key, tomb| {
                assert!(!tomb);
                walked.push(key.rowid);
            })
            .unwrap()
        {
            lo = next;
        }
        assert_eq!(walked, written);

        // The cursor turns the next leaf's completed index response into its
        // first data-block prefetch before it crosses that leaf boundary.
        let second_leaf_at = 8 + VERSIONED_ROOT_ENTRY;
        let mut second_leaf = [0u8; 32];
        second_leaf.copy_from_slice(&buf[second_leaf_at + 20..second_leaf_at + 52]);
        let second_leaf = BlockId(second_leaf);
        s.get(&second_leaf, &mut buf).unwrap();
        let second_leaf_first_data = block_ref_at(&buf, 0, false).id();
        let mut lookahead = LookaheadStore {
            inner: s,
            requested: Vec::new(),
            prefetched: Vec::new(),
        };
        let mut cursor = SstCursor::new(handle);
        let mut index = vec![0u8; MAX_PAYLOAD];
        let mut data = vec![0u8; MAX_PAYLOAD];
        let mut bounce = vec![0u8; MAX_PAYLOAD];
        let mut copy = vec![0u8; 128 * 1024];
        let mut cursor_rows = 0;
        while cursor
            .next_copy(
                &mut lookahead,
                &mut index,
                &mut data,
                &mut bounce,
                &mut copy,
            )
            .unwrap()
            .is_some()
        {
            cursor_rows += 1;
        }
        assert_eq!(cursor_rows, written.len());
        let leaf_at = lookahead
            .requested
            .iter()
            .position(|id| *id == second_leaf)
            .expect("second leaf scheduled");
        let data_at = lookahead
            .requested
            .iter()
            .position(|id| *id == second_leaf_first_data)
            .expect("first data block from second leaf scheduled");
        assert!(
            leaf_at < data_at,
            "leaf completion must reveal its data prefetch"
        );
    }

    #[test]
    fn compressible_blocks_store_fewer_bytes_than_raw() {
        // Repetitive rows must land as LZ4 blocks: the store's used bytes
        // stay well under the raw payload total, and every row still reads
        // back exactly — meaning the whole read path decompresses.
        let (_b, mut s) = store();
        let a = arena();
        let rows: Vec<_> = (1..=40u64)
            .map(|i| {
                (
                    i,
                    format!("row {i} says the same thing over and over; ")
                        .repeat(400)
                        .into_bytes(),
                )
            })
            .collect();
        let raw_total: usize = rows.iter().map(|(_, r)| r.len()).sum();
        let root = build(&mut s, &rows).expect("root");
        assert!(
            s.used() < raw_total / 3,
            "{} bytes stored for {raw_total} raw",
            s.used()
        );
        let mut r = SstReader::new(&a).unwrap();
        for (rowid, row) in &rows {
            assert_eq!(
                get(&mut r, &mut s, &root, *rowid).as_ref(),
                Some(row),
                "row {rowid}"
            );
        }
    }

    #[test]
    fn variable_row_sizes_in_one_block_are_read_back() {
        let (_b, mut s) = store();
        let a = arena();
        let rows: Vec<_> = (1..=20u64)
            .map(|i| (i, vec![i as u8; (i * 3) as usize]))
            .collect();
        let root = build(&mut s, &rows).expect("root");
        let mut r = SstReader::new(&a).unwrap();
        for (rowid, row) in &rows {
            assert_eq!(
                get(&mut r, &mut s, &root, *rowid).as_ref(),
                Some(row),
                "row {rowid}"
            );
        }
    }
}
