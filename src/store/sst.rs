//! A sorted string table over the block grid.
//!
//! An SST is a table's rows written once, in key order, and never changed —
//! which is what lets it be a run of immutable blocks rather than a file that
//! is seeked within. Rows are packed into [`BlockType::SstData`] blocks in key
//! order, and a single [`BlockType::SstIndex`] block records, for each data
//! block, the first key it holds and the block's identity. The index is the
//! SST's root: given its identity a reader can find any key, and given the root
//! nothing else about the SST needs naming.
//!
//! The index is *sparse* — one entry per data block, not per row. Finding a key
//! is a binary search of the index for the last block whose first key does not
//! exceed the target, then a scan of that one block. So a lookup reads exactly
//! two blocks whatever the table's size: the index and the data block the key
//! must be in if it is anywhere. That is the whole point of the sparse index —
//! it is small enough to cache and to ship to the bucket alongside the data,
//! the way Loki ships its chunk index.
//!
//! Keys are row identities (`u64`), matching what the current checkpoint SST is
//! keyed by, so this re-expresses that format in blocks rather than inventing a
//! new key space. Rows within a block and blocks within the SST are both in
//! ascending key order, which is what makes the two binary searches valid.

use crate::mem::arena::Arena;

use super::bloom::{self, FILTER_BYTES};
use super::{BlockId, BlockStore, BlockType, StoreError, MAX_PAYLOAD};

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
}

/// `rowid` u64 | `len` u32, then the row bytes — one row inside a data block.
/// `len`'s high bit marks a *chained* entry: a row too large for one block,
/// whose payload continues in overflow blocks. The masked low bits are the
/// row's total length; the entry body is then `n_chunks` u16, the overflow
/// blocks' identities, and the head chunk inline.
const ENTRY_HEADER: usize = 12;

/// High bit of the entry length: the row continues in overflow blocks.
const CHAIN_FLAG: u32 = 1 << 31;

/// Tombstone bit: the entry records a deletion, not a row — a delta SST's
/// way of saying an older SST's version of this key is gone. Carries no
/// payload.
const TOMB_FLAG: u32 = 1 << 30;

/// The largest assembled row a reader's scan scratch admits: the chained
/// head chunk plus every overflow block.
pub(crate) const MAX_ASSEMBLED: usize =
    (MAX_PAYLOAD - ENTRY_HEADER - 2 - MAX_CHAIN * 32) + MAX_CHAIN * MAX_PAYLOAD;

/// The most overflow blocks one chained row may span. With ~256 KiB blocks
/// this caps a single row at about 4 MiB — far above anything the engine's
/// arenas admit — and exceeding it is a loud error, never truncation.
const MAX_CHAIN: usize = 16;

/// `first_rowid` u64 | `block_id` [u8; 32] — one data block's index entry.
const INDEX_ENTRY: usize = 8 + 32;

/// The most data blocks a single-block index can point at. A larger SST needs a
/// multi-block index, which is a later concern; this bound is checked and raised
/// rather than silently overrun.
const MAX_DATA_BLOCKS: usize = MAX_PAYLOAD / INDEX_ENTRY;

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
    pending_first: Option<u64>,
    /// The index as it grows: `(first_rowid, block_id)` per flushed data block.
    index: Box<[(u64, BlockId)]>,
    index_len: usize,
    /// The last key written, so out-of-order rows are caught rather than
    /// producing an SST whose binary search silently misses them.
    last_key: Option<u64>,
    /// The filter's bit array, one key set into it per append. It becomes the
    /// filter block at finish.
    filter: Box<[u8]>,
    /// Every block identity written so far (data and chain blocks), for the
    /// roster.
    roster: Box<[BlockId]>,
    roster_len: usize,
}

impl SstWriter {
    /// Allocates the writer's fixed buffers (about 0.9 MiB). Startup only —
    /// after the freeze, `reset` is how a writer is reused.
    pub(crate) fn new() -> Self {
        Self {
            pending: vec![0u8; MAX_PAYLOAD].into_boxed_slice(),
            pending_len: 0,
            pending_first: None,
            index: vec![(0u64, BlockId([0u8; 32])); MAX_DATA_BLOCKS].into_boxed_slice(),
            index_len: 0,
            last_key: None,
            filter: vec![0u8; FILTER_BYTES].into_boxed_slice(),
            roster: vec![BlockId([0u8; 32]); MAX_ROSTER].into_boxed_slice(),
            roster_len: 0,
        }
    }

    /// The fixed bytes one writer reserves, for budget estimates.
    pub(crate) fn budget_bytes() -> usize {
        MAX_PAYLOAD
            + MAX_DATA_BLOCKS * core::mem::size_of::<(u64, BlockId)>()
            + FILTER_BYTES
            + MAX_ROSTER * 32
    }

    /// Empties the writer for its next SST. Allocation-free.
    pub(crate) fn reset(&mut self) {
        self.pending_len = 0;
        self.pending_first = None;
        self.index_len = 0;
        self.last_key = None;
        self.filter.fill(0);
        self.roster_len = 0;
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
        if let Some(last) = self.last_key
            && rowid <= last
        {
            return Err(SstError::KeyOutOfOrder);
        }
        let entry = ENTRY_HEADER + row.len();
        if entry > MAX_PAYLOAD {
            return self.append_chained(store, rowid, row);
        }
        if self.pending_len + entry > MAX_PAYLOAD {
            self.flush_data(store)?;
        }
        let at = self.pending_len;
        self.pending[at..at + 8].copy_from_slice(&rowid.to_le_bytes());
        self.pending[at + 8..at + 12].copy_from_slice(&(row.len() as u32).to_le_bytes());
        self.pending[at + 12..at + entry].copy_from_slice(row);
        self.pending_len += entry;
        if self.pending_first.is_none() {
            self.pending_first = Some(rowid);
        }
        self.last_key = Some(rowid);
        bloom::insert(&mut self.filter, rowid);
        Ok(())
    }

    /// A row too large for one block: its tail is written as overflow blocks
    /// first (so their identities are known), then a head entry — alone in its
    /// own data block — carries the chain's identities and the leading chunk.
    fn append_chained(
        &mut self,
        store: &mut dyn BlockStore,
        rowid: u64,
        row: &[u8],
    ) -> Result<(), SstError> {
        // The head block holds the entry header, the chunk count, up to
        // MAX_CHAIN identities, and the head chunk; overflow blocks are raw.
        let head_room = MAX_PAYLOAD - ENTRY_HEADER - 2 - MAX_CHAIN * 32;
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
        self.pending[at..at + 8].copy_from_slice(&rowid.to_le_bytes());
        self.pending[at + 8..at + 12]
            .copy_from_slice(&((row.len() as u32) | CHAIN_FLAG).to_le_bytes());
        let mut cursor = ENTRY_HEADER;
        self.pending[cursor..cursor + 2].copy_from_slice(&(n_chunks as u16).to_le_bytes());
        cursor += 2;
        for id in &ids[..n_chunks] {
            self.pending[cursor..cursor + 32].copy_from_slice(&id.0);
            cursor += 32;
        }
        self.pending[cursor..cursor + head_room].copy_from_slice(&row[..head_room]);
        self.pending_len = cursor + head_room;
        self.pending_first = Some(rowid);
        self.last_key = Some(rowid);
        bloom::insert(&mut self.filter, rowid);
        self.flush_data(store)
    }

    /// Appends a deletion marker for `rowid`. Ordered with the rows, sized
    /// like an empty entry.
    pub(crate) fn append_tombstone(
        &mut self,
        store: &mut dyn BlockStore,
        rowid: u64,
    ) -> Result<(), SstError> {
        if let Some(last) = self.last_key
            && rowid <= last
        {
            return Err(SstError::KeyOutOfOrder);
        }
        if self.pending_len + ENTRY_HEADER > MAX_PAYLOAD {
            self.flush_data(store)?;
        }
        let at = self.pending_len;
        self.pending[at..at + 8].copy_from_slice(&rowid.to_le_bytes());
        self.pending[at + 8..at + 12].copy_from_slice(&TOMB_FLAG.to_le_bytes());
        self.pending_len += ENTRY_HEADER;
        if self.pending_first.is_none() {
            self.pending_first = Some(rowid);
        }
        self.last_key = Some(rowid);
        // Not in the bloom filter: the filter answers "is this row here", and
        // a tombstone is exactly a row not being here.
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
            return Err(SstError::TooManyBlocks);
        }
        let first = self.pending_first.expect("a non-empty block has a first key");
        let id = store.put(&self.pending[..self.pending_len], BlockType::SstData, 0)?;
        self.record(id)?;
        self.index[self.index_len] = (first, id);
        self.index_len += 1;
        self.pending_len = 0;
        self.pending_first = None;
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
        if self.index_len == 0 {
            return Ok(None);
        }
        // The filter block, so a reader can skip this SST without the index.
        let filter = store.put(&self.filter, BlockType::SstFilter, 0)?;
        self.record(filter)?;
        // The index block: count, then (first_rowid, block_id) per data block.
        let bytes = 4 + self.index_len * INDEX_ENTRY;
        let buffer = &mut *self.pending; // reuse the data scratch; it is done with
        buffer[0..4].copy_from_slice(&(self.index_len as u32).to_le_bytes());
        for (i, (first, id)) in self.index[..self.index_len].iter().enumerate() {
            let at = 4 + i * INDEX_ENTRY;
            buffer[at..at + 8].copy_from_slice(&first.to_le_bytes());
            buffer[at + 8..at + INDEX_ENTRY].copy_from_slice(&id.0);
        }
        let index = store.put(&buffer[..bytes], BlockType::SstIndex, 0)?;
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
        for (i, id) in self.roster[..self.roster_len].iter().enumerate() {
            buffer[i * 32..i * 32 + 32].copy_from_slice(&id.0);
        }
        let roster = store.put(&buffer[..roster_bytes], BlockType::SstRoster, 0)?;
        Ok(Some(SstHandle { index, filter, roster }))
    }
}

/// Reads rows out of an SST by its root. Holds one block of scratch for the
/// index and one for a data block, so a lookup borrows no memory from the
/// caller beyond the buffer the row is copied into.
pub(crate) struct SstReader<'a> {
    index_scratch: &'a mut [u8],
    data_scratch: &'a mut [u8],
    /// Scratch a range scan assembles a chained row into (a point lookup
    /// assembles straight into the caller's buffer instead).
    assembly: &'a mut [u8],
}

impl<'a> SstReader<'a> {
    pub(crate) fn new(arena: &'a Arena) -> Result<Self, SstError> {
        let index_scratch = arena
            .alloc_slice_with(MAX_PAYLOAD, |_| 0u8)
            .map_err(|_| SstError::Store(StoreError::Unavailable))?;
        let data_scratch = arena
            .alloc_slice_with(MAX_PAYLOAD, |_| 0u8)
            .map_err(|_| SstError::Store(StoreError::Unavailable))?;
        let assembly = arena
            .alloc_slice_with(MAX_ASSEMBLED, |_| 0u8)
            .map_err(|_| SstError::Store(StoreError::Unavailable))?;
        Ok(Self { index_scratch, data_scratch, assembly })
    }

    /// A reader over caller-owned buffers — the long-lived spill path, whose
    /// scratch persists across statements instead of living in an arena.
    /// `index`/`data` must each hold a block payload; `assembly` a chained row.
    pub(crate) fn over(
        index: &'a mut [u8],
        data: &'a mut [u8],
        assembly: &'a mut [u8],
    ) -> Self {
        Self { index_scratch: index, data_scratch: data, assembly }
    }

    /// Finds `rowid`, copying its row into `into` and returning the length, or
    /// `None` when the SST does not hold it. Checks the filter first: a key the
    /// filter rejects returns without the index or a data block being read at
    /// all. A key it admits reads the index and the one data block the key
    /// would be in — two blocks, as before, plus the filter.
    pub(crate) fn get(
        &mut self,
        store: &mut dyn BlockStore,
        handle: &SstHandle,
        rowid: u64,
        into: &mut [u8],
    ) -> Result<Option<usize>, SstError> {
        // The filter reuses the index buffer: it is consulted and done with
        // before the index is read, so the two never coexist.
        let filter_len = store.get(&handle.filter, self.index_scratch)?;
        if !bloom::maybe_contains(&self.index_scratch[..filter_len], rowid) {
            return Ok(None);
        }
        let count = self.load_index(store, &handle.index)?;
        let Some(entry) = block_containing(self.index_scratch, count, rowid) else {
            return Ok(None);
        };
        let block_id = block_id_at(self.index_scratch, entry);

        // Scan the one data block for the row. The block is small and bounded,
        // so a linear scan of it is the read the sparse index traded for not
        // indexing every row.
        let data_len = store.get(&block_id, self.data_scratch)?;
        let mut found: Option<(usize, bool)> = None;
        for entry in DataBlock(&self.data_scratch[..data_len]) {
            if entry.key == rowid {
                if entry.tombstone {
                    break;
                }
                if entry.is_chained() {
                    assemble_chain(store, &entry, into)?;
                    found = Some((entry.total_len, true));
                } else {
                    if into.len() < entry.total_len {
                        return Err(SstError::Store(StoreError::BufferTooSmall));
                    }
                    into[..entry.total_len].copy_from_slice(entry.head);
                    found = Some((entry.total_len, false));
                }
                break;
            }
            // Rows are ascending, so once past the target it is not here.
            if entry.key > rowid {
                break;
            }
        }
        Ok(found.map(|(n, _)| n))
    }

    /// Whether the SST holds `rowid`, without copying its bytes: `None` —
    /// absent; `Some(None)` — a tombstone; `Some(Some(len))` — a live row of
    /// `len` bytes. The filter and index gate the read exactly as `get`
    /// does; this is the existence probe the row-map overlay answers point
    /// lookups with.
    pub(crate) fn probe(
        &mut self,
        store: &mut dyn BlockStore,
        handle: &SstHandle,
        rowid: u64,
    ) -> Result<Option<Option<u32>>, SstError> {
        let filter_len = store.get(&handle.filter, self.index_scratch)?;
        if !bloom::maybe_contains(&self.index_scratch[..filter_len], rowid) {
            return Ok(None);
        }
        let count = self.load_index(store, &handle.index)?;
        let Some(entry) = block_containing(self.index_scratch, count, rowid) else {
            return Ok(None);
        };
        let block_id = block_id_at(self.index_scratch, entry);
        let data_len = store.get(&block_id, self.data_scratch)?;
        for entry in DataBlock(&self.data_scratch[..data_len]) {
            if entry.key == rowid {
                return Ok(Some(if entry.tombstone {
                    None
                } else {
                    Some(entry.total_len as u32)
                }));
            }
            if entry.key > rowid {
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
        // help here; the index locates the covering blocks directly.
        let count = self.load_index(store, &handle.index)?;
        // The block `lo` falls in, or — when `lo` precedes every key — the
        // first block, since the range may still cover it from the left.
        let start = block_containing(self.index_scratch, count, lo).unwrap_or(0);
        for entry_index in start..count {
            let block_id = block_id_at(self.index_scratch, entry_index);
            let data_len = store.get(&block_id, self.data_scratch)?;
            let mut ran_past = false;
            // A chained entry owns its whole block, so at most one assembly
            // happens per block and the borrow of `data_scratch` has ended by
            // the time the chain's overflow blocks are read.
            let mut chained: Option<(u64, usize)> = None;
            for entry in DataBlock(&self.data_scratch[..data_len]) {
                if entry.key > hi {
                    ran_past = true;
                    break;
                }
                if entry.key >= lo {
                    if entry.tombstone {
                        emit(entry.key, None);
                    } else if entry.is_chained() {
                        assemble_chain(store, &entry, self.assembly)?;
                        chained = Some((entry.key, entry.total_len));
                        break;
                    } else {
                        emit(entry.key, Some(entry.head));
                    }
                }
            }
            if let Some((key, n)) = chained {
                emit(key, Some(&self.assembly[..n]));
            }
            // A block ending past `hi` bounds the scan: later blocks hold only
            // larger keys, so none of them can be in range.
            if ran_past {
                break;
            }
        }
        Ok(())
    }

    /// A bounded slice of a full scan: streams rows with keys `>= lo`, in key
    /// order, reading at most `max_blocks` data blocks, and returns where to
    /// resume — `Some(next_lo)` when the SST has more, `None` when it is
    /// exhausted. This is what lets a paced merge pay for its schedule a few
    /// block reads per beat instead of one unbounded pass.
    pub(crate) fn scan_bounded(
        &mut self,
        store: &mut dyn BlockStore,
        handle: &SstHandle,
        lo: u64,
        max_blocks: usize,
        emit: &mut dyn FnMut(u64, bool),
    ) -> Result<Option<u64>, SstError> {
        let count = self.load_index(store, &handle.index)?;
        let start = block_containing(self.index_scratch, count, lo).unwrap_or(0);
        let end = (start + max_blocks).min(count);
        let mut last_key: Option<u64> = None;
        for entry_index in start..end {
            let block_id = block_id_at(self.index_scratch, entry_index);
            let data_len = store.get(&block_id, self.data_scratch)?;
            for entry in DataBlock(&self.data_scratch[..data_len]) {
                if entry.key >= lo {
                    emit(entry.key, entry.tombstone);
                }
                last_key = Some(last_key.map_or(entry.key, |k| k.max(entry.key)));
            }
        }
        if end == count {
            return Ok(None);
        }
        // Resuming one past the largest key seen re-locates through the index
        // and cannot skip an entry: keys are unique and ascending.
        Ok(Some(last_key.map_or(lo, |k| k + 1)))
    }

    /// Reads and validates the index block, returning its data-block count.
    fn load_index(&mut self, store: &mut dyn BlockStore, root: &BlockId) -> Result<usize, SstError> {
        store.get(root, self.index_scratch)?;
        Ok(u32::from_le_bytes(self.index_scratch[0..4].try_into().unwrap()) as usize)
    }
}

/// Binary-searches the sparse index for the last block whose first key does not
/// exceed `key` — the only data block `key` can be in. `None` when the index is
/// empty or `key` precedes every block's first key.
fn block_containing(index: &[u8], count: usize, key: u64) -> Option<usize> {
    let first_key = |i: usize| {
        let at = 4 + i * INDEX_ENTRY;
        u64::from_le_bytes(index[at..at + 8].try_into().unwrap())
    };
    if count == 0 || key < first_key(0) {
        return None;
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
    Some(lo)
}

/// The block identity stored in index entry `i`.
fn block_id_at(index: &[u8], i: usize) -> BlockId {
    let mut id = [0u8; 32];
    id.copy_from_slice(&index[4 + i * INDEX_ENTRY + 8..4 + i * INDEX_ENTRY + INDEX_ENTRY]);
    BlockId(id)
}

/// One row read out of a data block. For an ordinary entry `head` is the
/// whole row and `chain` is empty; a chained entry's `head` is the leading
/// chunk and `chain` the overflow blocks' identities (32 bytes each), with
/// `total_len` the assembled row's length.
struct DataEntry<'a> {
    key: u64,
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
struct DataBlock<'a>(&'a [u8]);

impl<'a> Iterator for DataBlock<'a> {
    type Item = DataEntry<'a>;

    fn next(&mut self) -> Option<DataEntry<'a>> {
        let data = self.0;
        if data.len() < ENTRY_HEADER {
            return None;
        }
        let key = u64::from_le_bytes(data[0..8].try_into().unwrap());
        let raw_len = u32::from_le_bytes(data[8..12].try_into().unwrap());
        if raw_len & TOMB_FLAG != 0 {
            self.0 = &data[ENTRY_HEADER..];
            return Some(DataEntry { key, total_len: 0, head: &[], chain: &[], tombstone: true });
        }
        if raw_len & CHAIN_FLAG != 0 {
            // A chained head fills the rest of its block: count, identities,
            // then the leading chunk.
            let total_len = (raw_len & !CHAIN_FLAG) as usize;
            let body = &data[ENTRY_HEADER..];
            if body.len() < 2 {
                return None;
            }
            let n_chunks = u16::from_le_bytes(body[0..2].try_into().unwrap()) as usize;
            if n_chunks > MAX_CHAIN || body.len() < 2 + n_chunks * 32 {
                return None;
            }
            let chain = &body[2..2 + n_chunks * 32];
            let head = &body[2 + n_chunks * 32..];
            self.0 = &[];
            return Some(DataEntry { key, total_len, head, chain, tombstone: false });
        }
        let len = raw_len as usize;
        if data.len() < ENTRY_HEADER + len {
            return None;
        }
        self.0 = &data[ENTRY_HEADER + len..];
        Some(DataEntry {
            key,
            total_len: len,
            head: &data[ENTRY_HEADER..ENTRY_HEADER + len],
            chain: &[],
            tombstone: false,
        })
    }
}

/// One `(rowid, tombstone, total_len, next_offset)` step through a data
/// block's entries without copying row bytes — how the row-map overlay's
/// merged enumeration walks keys. `at` is the previous step's returned
/// offset (0 to start); `None` is the block's end.
pub(crate) fn block_keys_at(block: &[u8], at: usize) -> Option<(u64, bool, u32, usize)> {
    if at >= block.len() {
        return None;
    }
    let remaining = &block[at..];
    let before = remaining.len();
    let mut entries = DataBlock(remaining);
    let entry = entries.next()?;
    let consumed = before - entries.0.len();
    Some((entry.key, entry.tombstone, entry.total_len as u32, at + consumed))
}

/// The data-block count a fetched index block names.
pub(crate) fn index_block_count(index_block: &[u8]) -> usize {
    u32::from_le_bytes(index_block[0..4].try_into().unwrap()) as usize
}

/// The `i`th data block's identity in a fetched index block.
pub(crate) fn index_block_id(index_block: &[u8], i: usize) -> BlockId {
    block_id_at(index_block, i)
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
        let n = store.get(&BlockId(id), &mut into[at..])?;
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
        reader.get(store, handle, rowid, &mut out).unwrap().map(|n| {
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
        assert_eq!(get(&mut r, &mut s, &root, 1).as_deref(), Some(&b"only row"[..]));
        assert_eq!(get(&mut r, &mut s, &root, 2), None);
        assert_eq!(get(&mut r, &mut s, &root, 0), None);
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
        let rows: Vec<_> = (0..5000u64).map(|i| (i * 2 + 1, vec![i as u8; 400])).collect();
        let root = build(&mut s, &rows).expect("has a root");
        let mut r = SstReader::new(&a).unwrap();
        for (rowid, row) in &rows {
            assert_eq!(get(&mut r, &mut s, &root, *rowid).as_ref(), Some(row), "row {rowid}");
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
            assert_eq!(get(&mut r, &mut s, &root, probe), None, "odd key {probe} is absent");
            if s.reads() - before == 1 {
                single_read += 1;
            }
        }
        assert!(single_read >= 95, "the filter skipped the index on {single_read} of 100 absent keys");
    }

    #[test]
    fn rows_out_of_order_are_refused() {
        let (_b, mut s) = store();
        let mut w = SstWriter::new();
        w.append(&mut s, 5, b"five").unwrap();
        assert_eq!(w.append(&mut s, 3, b"three").err(), Some(SstError::KeyOutOfOrder));
        assert_eq!(w.append(&mut s, 5, b"again").err(), Some(SstError::KeyOutOfOrder));
    }

    #[test]
    fn a_row_larger_than_a_block_chains_and_round_trips() {
        // A row past one block's payload spans overflow blocks and reads back
        // byte-identical, by point lookup and by scan, with ordinary rows on
        // both sides of it.
        let (_b, mut s) = store();
        let a = Arena::new(&mut Budget::new(64 << 20), "sst chain", 16 << 20).expect("arena");
        let mut w = SstWriter::new();
        let huge: Vec<u8> = (0..MAX_PAYLOAD + 50_000).map(|i| (i * 31 % 251) as u8).collect();
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
        assert_eq!(r.get(&mut s, &root, 2, &mut out).unwrap(), None, "tombstoned key is absent");
        assert!(r.get(&mut s, &root, 1, &mut out).unwrap().is_some());
        let mut seen = Vec::new();
        r.scan(&mut s, &root, 0, u64::MAX, &mut |k, row| seen.push((k, row.is_none())))
            .unwrap();
        assert_eq!(seen, vec![(1, false), (2, true), (3, false)]);
    }

    #[test]
    fn a_row_beyond_the_chain_bound_is_refused() {
        let (_b, mut s) = store();
        let mut w = SstWriter::new();
        let huge = vec![0u8; MAX_ASSEMBLED + MAX_PAYLOAD];
        assert_eq!(w.append(&mut s, 1, &huge).err(), Some(SstError::RowTooLarge));
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
        let rows: Vec<_> = (0..4000u64).map(|i| (i, vec![(i % 251) as u8; 400])).collect();
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
        assert_eq!(scan(&mut r, &mut s, &root, 0, 5).len(), 0, "before the first key");
        assert_eq!(scan(&mut r, &mut s, &root, 40, 99).len(), 0, "after the last key");
        assert_eq!(scan(&mut r, &mut s, &root, 0, 100).len(), 21, "covers everything");
        let straddle_low = scan(&mut r, &mut s, &root, 5, 12);
        assert_eq!(straddle_low.iter().map(|(k, _)| *k).collect::<Vec<_>>(), vec![10, 11, 12]);
        let straddle_high = scan(&mut r, &mut s, &root, 28, 50);
        assert_eq!(straddle_high.iter().map(|(k, _)| *k).collect::<Vec<_>>(), vec![28, 29, 30]);
    }

    #[test]
    fn a_single_key_range_returns_just_that_row() {
        let (_b, mut s) = store();
        let a = arena();
        let rows: Vec<_> = (1..=40u64).map(|i| (i * 3, vec![i as u8; 16])).collect();
        let root = build(&mut s, &rows).expect("root");
        let mut r = SstReader::new(&a).unwrap();
        assert_eq!(scan(&mut r, &mut s, &root, 30, 30), vec![(30, vec![10u8; 16])]);
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
        assert_eq!(scan(&mut r, &mut s, &root, 8, 3), vec![], "hi below lo yields nothing");
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
        assert!(read <= 4, "a narrow range read {read} blocks; expected the index and a few data blocks");
    }

    #[test]
    fn variable_row_sizes_in_one_block_are_read_back() {
        let (_b, mut s) = store();
        let a = arena();
        let rows: Vec<_> = (1..=20u64).map(|i| (i, vec![i as u8; (i * 3) as usize])).collect();
        let root = build(&mut s, &rows).expect("root");
        let mut r = SstReader::new(&a).unwrap();
        for (rowid, row) in &rows {
            assert_eq!(get(&mut r, &mut s, &root, *rowid).as_ref(), Some(row), "row {rowid}");
        }
    }
}
