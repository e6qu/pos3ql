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

use super::{BlockId, BlockStore, BlockType, StoreError, MAX_PAYLOAD};

/// `rowid` u64 | `len` u32, then the row bytes — one row inside a data block.
const ENTRY_HEADER: usize = 12;

/// `first_rowid` u64 | `block_id` [u8; 32] — one data block's index entry.
const INDEX_ENTRY: usize = 8 + 32;

/// The most data blocks a single-block index can point at. A larger SST needs a
/// multi-block index, which is a later concern; this bound is checked and raised
/// rather than silently overrun.
const MAX_DATA_BLOCKS: usize = MAX_PAYLOAD / INDEX_ENTRY;

/// Building an SST failed. Distinct from [`StoreError`] because these are the
/// writer's own limits (a row too big for a block, more blocks than the index
/// can hold), not the store's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SstError {
    /// A single row does not fit in a data block.
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
/// The writer holds one block's worth of scratch and the index as it grows;
/// both are arena-allocated, so building an SST reserves its working memory
/// from the caller's arena rather than the heap.
pub(crate) struct SstWriter<'a> {
    /// Rows accumulating for the current data block.
    pending: &'a mut [u8],
    pending_len: usize,
    /// The first key in the current data block, set when its first row lands.
    pending_first: Option<u64>,
    /// The index as it grows: `(first_rowid, block_id)` per flushed data block.
    index: &'a mut [(u64, BlockId)],
    index_len: usize,
    /// The last key written, so out-of-order rows are caught rather than
    /// producing an SST whose binary search silently misses them.
    last_key: Option<u64>,
}

impl<'a> SstWriter<'a> {
    pub(crate) fn new(arena: &'a Arena) -> Result<Self, SstError> {
        let pending = arena
            .alloc_slice_with(MAX_PAYLOAD, |_| 0u8)
            .map_err(|_| SstError::Store(StoreError::Unavailable))?;
        let index = arena
            .alloc_slice_with(MAX_DATA_BLOCKS, |_| (0u64, BlockId([0u8; 32])))
            .map_err(|_| SstError::Store(StoreError::Unavailable))?;
        Ok(Self {
            pending,
            pending_len: 0,
            pending_first: None,
            index,
            index_len: 0,
            last_key: None,
        })
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
        let entry = ENTRY_HEADER + row.len();
        if entry > MAX_PAYLOAD {
            return Err(SstError::RowTooLarge);
        }
        if let Some(last) = self.last_key
            && rowid <= last
        {
            return Err(SstError::KeyOutOfOrder);
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
        mut self,
        store: &mut dyn BlockStore,
    ) -> Result<Option<BlockId>, SstError> {
        self.flush_data(store)?;
        if self.index_len == 0 {
            return Ok(None);
        }
        // The index block: count, then (first_rowid, block_id) per data block.
        let bytes = 4 + self.index_len * INDEX_ENTRY;
        let buffer = &mut *self.pending; // reuse the data scratch; it is done with
        buffer[0..4].copy_from_slice(&(self.index_len as u32).to_le_bytes());
        for (i, (first, id)) in self.index[..self.index_len].iter().enumerate() {
            let at = 4 + i * INDEX_ENTRY;
            buffer[at..at + 8].copy_from_slice(&first.to_le_bytes());
            buffer[at + 8..at + INDEX_ENTRY].copy_from_slice(&id.0);
        }
        let root = store.put(&buffer[..bytes], BlockType::SstIndex, 0)?;
        Ok(Some(root))
    }
}

/// Reads rows out of an SST by its root. Holds one block of scratch for the
/// index and one for a data block, so a lookup borrows no memory from the
/// caller beyond the buffer the row is copied into.
pub(crate) struct SstReader<'a> {
    index_scratch: &'a mut [u8],
    data_scratch: &'a mut [u8],
}

impl<'a> SstReader<'a> {
    pub(crate) fn new(arena: &'a Arena) -> Result<Self, SstError> {
        let index_scratch = arena
            .alloc_slice_with(MAX_PAYLOAD, |_| 0u8)
            .map_err(|_| SstError::Store(StoreError::Unavailable))?;
        let data_scratch = arena
            .alloc_slice_with(MAX_PAYLOAD, |_| 0u8)
            .map_err(|_| SstError::Store(StoreError::Unavailable))?;
        Ok(Self { index_scratch, data_scratch })
    }

    /// Finds `rowid`, copying its row into `into` and returning the length, or
    /// `None` when the SST does not hold it. Reads two blocks: the index, then
    /// the one data block the key would be in.
    pub(crate) fn get(
        &mut self,
        store: &mut dyn BlockStore,
        root: &BlockId,
        rowid: u64,
        into: &mut [u8],
    ) -> Result<Option<usize>, SstError> {
        let index_len = store.get(root, self.index_scratch)?;
        let index = &self.index_scratch[..index_len];
        let count = u32::from_le_bytes(index[0..4].try_into().unwrap()) as usize;

        // Binary search the sparse index for the last block whose first key does
        // not exceed the target: that is the only block the key can be in.
        let first_key = |i: usize| {
            let at = 4 + i * INDEX_ENTRY;
            u64::from_le_bytes(index[at..at + 8].try_into().unwrap())
        };
        if count == 0 || rowid < first_key(0) {
            return Ok(None);
        }
        let (mut lo, mut hi) = (0usize, count - 1);
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if first_key(mid) <= rowid {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        let mut block_id = [0u8; 32];
        block_id.copy_from_slice(&index[4 + lo * INDEX_ENTRY + 8..4 + lo * INDEX_ENTRY + INDEX_ENTRY]);
        let block_id = BlockId(block_id);

        // Scan the one data block for the row. The block is small and bounded,
        // so a linear scan of it is the read the sparse index traded for not
        // indexing every row.
        let data_len = store.get(&block_id, self.data_scratch)?;
        let data = &self.data_scratch[..data_len];
        let mut at = 0usize;
        while at + ENTRY_HEADER <= data.len() {
            let key = u64::from_le_bytes(data[at..at + 8].try_into().unwrap());
            let len = u32::from_le_bytes(data[at + 8..at + 12].try_into().unwrap()) as usize;
            if key == rowid {
                if into.len() < len {
                    return Err(SstError::Store(StoreError::BufferTooSmall));
                }
                into[..len].copy_from_slice(&data[at + ENTRY_HEADER..at + ENTRY_HEADER + len]);
                return Ok(Some(len));
            }
            // Rows are ascending, so once past the target it is not here.
            if key > rowid {
                break;
            }
            at += ENTRY_HEADER + len;
        }
        Ok(None)
    }
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
    fn build(store: &mut MemoryBlockStore, arena: &Arena, rows: &[(u64, Vec<u8>)]) -> Option<BlockId> {
        let mut w = SstWriter::new(arena).unwrap();
        for (rowid, row) in rows {
            w.append(store, *rowid, row).unwrap();
        }
        w.finish(store).unwrap()
    }

    fn get(
        reader: &mut SstReader,
        store: &mut MemoryBlockStore,
        root: &BlockId,
        rowid: u64,
    ) -> Option<Vec<u8>> {
        let mut out = vec![0u8; MAX_PAYLOAD];
        reader.get(store, root, rowid, &mut out).unwrap().map(|n| {
            out.truncate(n);
            out
        })
    }

    #[test]
    fn one_row_round_trips() {
        let (_b, mut s) = store();
        let a = arena();
        let root = build(&mut s, &a, &[(1, b"only row".to_vec())]).expect("has a root");
        let mut r = SstReader::new(&a).unwrap();
        assert_eq!(get(&mut r, &mut s, &root, 1).as_deref(), Some(&b"only row"[..]));
        assert_eq!(get(&mut r, &mut s, &root, 2), None);
        assert_eq!(get(&mut r, &mut s, &root, 0), None);
    }

    #[test]
    fn an_empty_sst_has_no_root() {
        let (_b, mut s) = store();
        let a = arena();
        assert_eq!(build(&mut s, &a, &[]), None);
    }

    #[test]
    fn every_row_is_found_across_many_data_blocks() {
        // Rows large enough that thousands span many data blocks, so the sparse
        // index and its binary search are actually exercised rather than a
        // single-block SST that never consults the index arithmetic.
        let (_b, mut s) = store();
        let a = arena();
        let rows: Vec<_> = (0..5000u64).map(|i| (i * 2 + 1, vec![i as u8; 400])).collect();
        let root = build(&mut s, &a, &rows).expect("has a root");
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
    fn a_lookup_reads_exactly_two_blocks() {
        // The sparse index's guarantee: whatever the SST's size, a hit costs the
        // index block plus one data block, no more.
        let (_b, mut s) = store();
        let a = arena();
        let rows: Vec<_> = (0..3000u64).map(|i| (i + 1, vec![7u8; 500])).collect();
        let root = build(&mut s, &a, &rows).expect("has a root");
        let mut r = SstReader::new(&a).unwrap();
        let before = s.reads();
        let _ = get(&mut r, &mut s, &root, 2500);
        assert_eq!(s.reads() - before, 2, "a lookup should read the index and one data block");
    }

    #[test]
    fn rows_out_of_order_are_refused() {
        let (_b, mut s) = store();
        let a = arena();
        let mut w = SstWriter::new(&a).unwrap();
        w.append(&mut s, 5, b"five").unwrap();
        assert_eq!(w.append(&mut s, 3, b"three").err(), Some(SstError::KeyOutOfOrder));
        assert_eq!(w.append(&mut s, 5, b"again").err(), Some(SstError::KeyOutOfOrder));
    }

    #[test]
    fn a_row_too_large_for_a_block_is_refused() {
        let (_b, mut s) = store();
        let a = arena();
        let mut w = SstWriter::new(&a).unwrap();
        let huge = vec![0u8; MAX_PAYLOAD];
        assert_eq!(w.append(&mut s, 1, &huge).err(), Some(SstError::RowTooLarge));
    }

    #[test]
    fn a_short_output_buffer_is_refused() {
        let (_b, mut s) = store();
        let a = arena();
        let root = build(&mut s, &a, &[(1, vec![9u8; 100])]).expect("root");
        let mut r = SstReader::new(&a).unwrap();
        let mut small = [0u8; 10];
        assert_eq!(
            r.get(&mut s, &root, 1, &mut small).err(),
            Some(SstError::Store(StoreError::BufferTooSmall))
        );
    }

    #[test]
    fn variable_row_sizes_in_one_block_are_read_back() {
        let (_b, mut s) = store();
        let a = arena();
        let rows: Vec<_> = (1..=20u64).map(|i| (i, vec![i as u8; (i * 3) as usize])).collect();
        let root = build(&mut s, &a, &rows).expect("root");
        let mut r = SstReader::new(&a).unwrap();
        for (rowid, row) in &rows {
            assert_eq!(get(&mut r, &mut s, &root, *rowid).as_ref(), Some(row), "row {rowid}");
        }
    }
}
