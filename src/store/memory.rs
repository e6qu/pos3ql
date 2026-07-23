//! A RAM-resident block store over a fixed byte budget.
//!
//! This is the tier the read path hits first, and the one the tests use to
//! exercise the seam without a bucket. Its budget is drawn once at startup:
//! blocks land in a single reserved slab and an index maps identity to extent,
//! so a full store is a loud error rather than growth.
//!
//! A full store raises `Unavailable` and keeps every block it already holds. It
//! never reuses room by dropping one, because that would make "present" mean
//! "present unless something else needed the space" — the right meaning for a
//! *cache*, the wrong one for a *store*, and the difference decides whether a
//! caller still owes the bucket an upload. Stage B's cache is the layer that
//! reclaims space, and it sits in front of this rather than inside it.

use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_map::FixedMap;

use super::{decode, encode, BlockId, BlockStore, BlockType, StoreError};

/// Where one block's bytes sit in the slab.
#[derive(Clone, Copy)]
struct Extent {
    at: usize,
    len: usize,
}

pub(crate) struct MemoryBlockStore {
    slab: Box<[u8]>,
    /// Successful reads, so a caller can prove how many blocks a lookup touched.
    reads: u64,
    /// Bytes handed out so far. Blocks are immutable and never removed, so the
    /// slab only ever grows towards its end.
    used: usize,
    index: FixedMap<BlockId, Extent>,
}

impl MemoryBlockStore {
    pub(crate) fn new(
        budget: &mut Budget,
        what: &'static str,
        bytes: usize,
        max_blocks: usize,
    ) -> Result<Self, BudgetError> {
        budget.draw_array(bytes, 1, what)?;
        let index = FixedMap::new(budget, what, max_blocks)?;
        let slab = vec![0u8; bytes];
        Ok(Self { slab: slab.into_boxed_slice(), reads: 0, used: 0, index })
    }

    pub(crate) fn len(&self) -> usize {
        self.index.len()
    }

    /// Blocks read back so far.
    pub(crate) fn reads(&self) -> u64 {
        self.reads
    }

    /// Bytes still available for new blocks.
    pub(crate) fn remaining(&self) -> usize {
        self.slab.len() - self.used
    }
}

impl BlockStore for MemoryBlockStore {
    fn put(
        &mut self,
        payload: &[u8],
        block_type: BlockType,
        lsn: u64,
    ) -> Result<BlockId, StoreError> {
        // Framed at the tail of the free space first, so a block that does not
        // fit is discovered before anything is committed to the index.
        let at = self.used;
        let (id, n) = encode(payload, block_type, lsn, &mut self.slab[at..])
            .map_err(|_| StoreError::Unavailable)?;
        // Already present: the identity is the content, so this is the same
        // block and the write has happened. The bytes just framed are dropped
        // by leaving `used` where it was.
        if self.index.get(&id).is_some() {
            return Ok(id);
        }
        self.index.insert(id, Extent { at, len: n }).map_err(|_| StoreError::Unavailable)?;
        self.used += n;
        Ok(id)
    }

    fn get(&mut self, id: &BlockId, into: &mut [u8]) -> Result<usize, StoreError> {
        let Some(extent) = self.index.get(id).copied() else {
            return Err(StoreError::NotFound);
        };
        // The bytes have not left this process, so the CRC is enough — it still
        // catches a stray write into the slab, while re-hashing every read
        // would pay for a substitution that cannot happen here.
        let block = decode(&self.slab[extent.at..extent.at + extent.len], false)?;
        if into.len() < block.payload.len() {
            return Err(StoreError::BufferTooSmall);
        }
        into[..block.payload.len()].copy_from_slice(block.payload);
        self.reads += 1;
        Ok(block.payload.len())
    }

    fn contains(&mut self, id: &BlockId) -> Result<bool, StoreError> {
        Ok(self.index.get(id).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(bytes: usize, blocks: usize) -> MemoryBlockStore {
        let mut budget = Budget::new(bytes + (1 << 20));
        MemoryBlockStore::new(&mut budget, "test blocks", bytes, blocks).expect("fits the budget")
    }

    #[test]
    fn round_trips_a_block() {
        let mut s = store(1 << 20, 16);
        let id = s.put(b"hello", BlockType::SstData, 3).unwrap();
        assert!(s.contains(&id).unwrap());
        let mut out = [0u8; 32];
        assert_eq!(s.get(&id, &mut out).unwrap(), 5);
        assert_eq!(&out[..5], b"hello");
    }

    #[test]
    fn writing_the_same_block_twice_stores_it_once() {
        // The property the whole design rests on: a retry after an ambiguous
        // failure costs nothing and leaves no duplicate.
        let mut s = store(1 << 20, 16);
        let first = s.put(b"same bytes", BlockType::SstData, 1).unwrap();
        let after_first = s.remaining();
        let second = s.put(b"same bytes", BlockType::SstData, 1).unwrap();
        assert_eq!(first, second);
        assert_eq!(s.len(), 1);
        assert_eq!(s.remaining(), after_first, "the retry consumed no room");
    }

    #[test]
    fn a_missing_block_is_not_found() {
        let mut s = store(1 << 20, 16);
        let absent = BlockId::of(b"never stored");
        assert!(!s.contains(&absent).unwrap());
        let mut out = [0u8; 32];
        assert_eq!(s.get(&absent, &mut out).err(), Some(StoreError::NotFound));
    }

    #[test]
    fn a_short_buffer_is_refused_rather_than_truncated() {
        let mut s = store(1 << 20, 16);
        let id = s.put(b"0123456789", BlockType::SstData, 1).unwrap();
        let mut out = [0u8; 4];
        assert_eq!(s.get(&id, &mut out).err(), Some(StoreError::BufferTooSmall));
    }

    #[test]
    fn a_full_store_is_a_loud_error() {
        // Exhaustion raises, keeping the slab's size and every block in it: a
        // stored block's presence must not depend on what was written after it.
        let mut s = store(4096, 64);
        let mut n = 0;
        loop {
            let payload = [n as u8; 512];
            match s.put(&payload, BlockType::SstData, n) {
                Ok(_) => n += 1,
                Err(e) => {
                    assert_eq!(e, StoreError::Unavailable);
                    break;
                }
            }
            assert!(n < 64, "the store never filled");
        }
        assert!(n > 0, "nothing fitted at all");
        // Everything written before the store filled is still readable.
        for i in 0..n {
            let id = BlockId::of(&[i as u8; 512]);
            assert!(s.contains(&id).unwrap(), "block {i} was dropped");
        }
    }

    #[test]
    fn damage_in_the_slab_is_caught_on_read() {
        let mut s = store(1 << 20, 16);
        let id = s.put(b"intact payload", BlockType::SstData, 1).unwrap();
        s.slab[super::super::HEADER_LEN] ^= 0xff;
        let mut out = [0u8; 32];
        assert!(matches!(s.get(&id, &mut out), Err(StoreError::Corrupt(_))));
    }
}
