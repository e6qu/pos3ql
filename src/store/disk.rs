//! The disk tier: a fixed-size cache file between RAM and the bucket.
//!
//! One preallocated file of equal-sized slots, an in-RAM index from block
//! identity to slot, and CLOCK eviction over the slots — the same discipline as
//! the WAL journal (a file sized once at startup, never grown) and the same
//! eviction as the RAM cache, one tier down. A read that missed the RAM frames
//! lands here before it has to cross the network.
//!
//! The file is pure cache. Every block in it is re-fetchable from the store
//! behind, so a slot that reads back wrong — a torn write from a crash
//! mid-update, a bit rot on the platter — is a miss, never data loss: the block
//! is dropped and re-fetched. That is what lets the slot header be small and the
//! write be a single positioned write with no fsync. A store could not reason
//! this way; a cache can, because losing a block here costs only the round trip
//! to fetch it again.
//!
//! A slot holds a framed block, header and all, exactly as [`super::encode`]
//! produced it. The block's own checksum and identity are what a read is
//! validated against, so a slot needs no integrity metadata of its own — the
//! block already carries it, and the identity is what proves the slot holds the
//! block the index thinks it does rather than a stale one a torn write left
//! behind.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_map::FixedMap;

use super::{decode, encode, BlockId, BlockStore, BlockType, StoreError, BLOCK_SIZE, MAX_PAYLOAD};

/// One slot holds a whole framed block. Fixed so a block's slot is its index
/// times this, with no allocation table to consult.
const SLOT_SIZE: usize = BLOCK_SIZE;

/// What occupies a slot, if anything.
#[derive(Clone, Copy)]
struct Slot {
    id: Option<BlockId>,
    len: usize,
    referenced: bool,
}

/// Hit/miss/eviction counts, and the reads a torn or rotted slot cost.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct DiskStats {
    pub(crate) hits: u64,
    pub(crate) misses: u64,
    pub(crate) evictions: u64,
    pub(crate) insertions: u64,
    /// Slots that failed to decode on read — a torn write or bit rot. Counted
    /// because a rising number is a sick disk, not a normal miss.
    pub(crate) corrupt_slots: u64,
}

pub(crate) struct DiskCache<S: BlockStore> {
    inner: S,
    file: File,
    slots: Box<[Slot]>,
    index: FixedMap<BlockId, usize>,
    /// Staging for one slot's bytes, in and out. The cache does not allocate on
    /// the read path, so the buffer a positioned read fills is reserved here.
    scratch: Box<[u8]>,
    hand: usize,
    stats: DiskStats,
}

impl<S: BlockStore> DiskCache<S> {
    /// Opens (creating if absent) a cache file of `slot_count` slots at `path`.
    /// The file is preallocated to its full size, as the journal is, so a slot
    /// write is only ever an overwrite and never extends the file.
    pub(crate) fn open(
        budget: &mut Budget,
        what: &'static str,
        inner: S,
        path: &Path,
        slot_count: usize,
    ) -> Result<Self, DiskError> {
        assert!(slot_count > 0, "a cache with no slots would miss on every read");
        budget.draw_array(slot_count, size_of::<Slot>(), what).map_err(DiskError::Budget)?;
        budget.draw_array(SLOT_SIZE, 1, what).map_err(DiskError::Budget)?;
        let index = FixedMap::new(budget, what, slot_count).map_err(DiskError::Budget)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| DiskError::Io("open cache file", e))?;
        // A previous run's file is discarded rather than trusted: its index is
        // gone with the process that built it, so its slots cannot be located,
        // and re-warming from the store is what a cache is for. Sizing to the
        // full extent up front makes every later write an overwrite.
        let capacity = (slot_count * SLOT_SIZE) as u64;
        file.set_len(capacity).map_err(|e| DiskError::Io("size cache file", e))?;
        Ok(Self {
            inner,
            file,
            slots: vec![Slot { id: None, len: 0, referenced: false }; slot_count]
                .into_boxed_slice(),
            index,
            scratch: vec![0u8; SLOT_SIZE].into_boxed_slice(),
            hand: 0,
            stats: DiskStats::default(),
        })
    }

    pub(crate) fn stats(&self) -> DiskStats {
        self.stats
    }

    /// How many slots a byte budget buys for the file itself. The in-RAM index
    /// and slot table are extra and drawn separately.
    pub(crate) fn slots_for(bytes: usize) -> usize {
        bytes / SLOT_SIZE
    }

    fn claim_slot(&mut self) -> usize {
        loop {
            let at = self.hand;
            self.hand = (self.hand + 1) % self.slots.len();
            if self.slots[at].referenced {
                self.slots[at].referenced = false;
                continue;
            }
            if let Some(evicted) = self.slots[at].id.take() {
                self.index.remove(&evicted);
                self.stats.evictions += 1;
            }
            return at;
        }
    }

    /// Frames `payload` and writes it to a slot. A write failure drops the block
    /// from the cache rather than raising: the block is already safe in the
    /// store, so a cache that could not keep a copy has only lost the reuse.
    fn admit(&mut self, id: BlockId, block_type: BlockType, lsn: u64, payload: &[u8]) {
        if payload.len() > MAX_PAYLOAD {
            return;
        }
        let Ok((_, n)) = encode(payload, block_type, lsn, &mut self.scratch) else {
            return;
        };
        let slot = self.claim_slot();
        if self.file.write_at(&self.scratch[..n], (slot * SLOT_SIZE) as u64).is_err() {
            // The slot is left empty; nothing points at a half-written block.
            return;
        }
        self.slots[slot] = Slot { id: Some(id), len: n, referenced: true };
        self.index.insert(id, slot).expect("index has a slot per file slot");
    }

    /// Drops a slot whose bytes did not read back as the block the index
    /// expected. A torn write or a rotted platter shows up here, and the block
    /// is re-fetched from the store, so the caller never sees the damage.
    fn drop_corrupt(&mut self, id: &BlockId, slot: usize) {
        self.slots[slot] = Slot { id: None, len: 0, referenced: false };
        self.index.remove(id);
        self.stats.corrupt_slots += 1;
    }
}

/// Opening the cache file failed, or its budget did not fit. Kept apart from
/// [`StoreError`] because these happen once at startup, not per block.
#[derive(Debug)]
pub(crate) enum DiskError {
    Io(&'static str, std::io::Error),
    Budget(BudgetError),
}

impl<S: BlockStore> BlockStore for DiskCache<S> {
    fn put(
        &mut self,
        payload: &[u8],
        block_type: BlockType,
        lsn: u64,
    ) -> Result<BlockId, StoreError> {
        let id = self.inner.put(payload, block_type, lsn)?;
        if self.index.get(&id).is_none() {
            self.admit(id, block_type, lsn, payload);
            self.stats.insertions += 1;
        }
        Ok(id)
    }

    fn get(&mut self, id: &BlockId, into: &mut [u8]) -> Result<usize, StoreError> {
        if let Some(&slot) = self.index.get(id) {
            let len = self.slots[slot].len;
            if self.file.read_at(&mut self.scratch[..len], (slot * SLOT_SIZE) as u64).is_ok() {
                // Verified, because the bytes came off a disk that a crash may
                // have torn. A mismatch is treated as a miss, not an error.
                if let Ok(block) = decode(&self.scratch[..len], true)
                    && block.id == *id
                {
                    if into.len() < block.payload.len() {
                        return Err(StoreError::BufferTooSmall);
                    }
                    into[..block.payload.len()].copy_from_slice(block.payload);
                    self.slots[slot].referenced = true;
                    self.stats.hits += 1;
                    return Ok(block.payload.len());
                }
            }
            // Torn, rotted, or holding some other block: drop it and fall
            // through to the store, which still has the real one.
            self.drop_corrupt(id, slot);
        }
        self.stats.misses += 1;
        let len = self.inner.get(id, into)?;
        // The block's frame metadata is not recoverable from the payload alone,
        // so a block fetched through here is re-framed as SstData at lsn 0 for
        // the slot. The header is the cache's own; the payload is what matters,
        // and it is verified against the identity on the next read regardless.
        self.admit(*id, BlockType::SstData, 0, &into[..len]);
        Ok(len)
    }

    fn contains(&mut self, id: &BlockId) -> Result<bool, StoreError> {
        if self.index.get(id).is_some() {
            return Ok(true);
        }
        self.inner.contains(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::memory::MemoryBlockStore;

    struct Fixture {
        cache: DiskCache<MemoryBlockStore>,
        _dir: std::path::PathBuf,
    }

    fn fixture(slots: usize) -> Fixture {
        // A unique path per test without Date/random: the slot count and a
        // process-stable counter distinguish them.
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pos3ql-diskcache-{}-{}", slots, n));
        let _ = std::fs::remove_file(&dir);
        let mut budget = Budget::new((slots + 4) * SLOT_SIZE + (16 << 20));
        let inner =
            MemoryBlockStore::new(&mut budget, "test store", 8 << 20, 512).expect("store fits");
        let cache = DiskCache::open(&mut budget, "test cache", inner, &dir, slots).expect("opens");
        Fixture { cache, _dir: dir }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self._dir);
        }
    }

    fn read(c: &mut DiskCache<MemoryBlockStore>, id: &BlockId) -> Vec<u8> {
        let mut out = vec![0u8; 4096];
        let n = c.get(id, &mut out).expect("reads");
        out.truncate(n);
        out
    }

    #[test]
    fn a_written_block_is_served_from_disk() {
        let mut f = fixture(8);
        let id = f.cache.put(b"to disk", BlockType::SstData, 1).unwrap();
        assert_eq!(read(&mut f.cache, &id), b"to disk");
        assert_eq!(f.cache.stats().hits, 1);
        assert_eq!(f.cache.stats().misses, 0);
    }

    #[test]
    fn a_miss_fetches_then_serves_from_disk() {
        let mut f = fixture(8);
        let id = f.cache.inner.put(b"cold", BlockType::SstData, 1).unwrap();
        assert_eq!(read(&mut f.cache, &id), b"cold");
        assert_eq!(f.cache.stats().misses, 1);
        assert_eq!(read(&mut f.cache, &id), b"cold");
        assert_eq!(f.cache.stats().hits, 1);
    }

    #[test]
    fn eviction_loses_nothing_because_the_store_still_has_it() {
        let mut f = fixture(2);
        let ids: Vec<_> = (0..8u8)
            .map(|i| f.cache.put(&[i; 128], BlockType::SstData, i as u64).unwrap())
            .collect();
        assert!(f.cache.stats().evictions > 0);
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(read(&mut f.cache, id), vec![i as u8; 128], "block {i} lost");
        }
    }

    #[test]
    fn a_torn_slot_is_a_miss_not_a_wrong_answer() {
        // The property that lets the file skip fsync: damaged bytes on disk
        // never reach the caller, because the block is re-fetched from the
        // store and the slot is dropped.
        let mut f = fixture(8);
        let id = f.cache.put(b"the true block", BlockType::SstData, 1).unwrap();
        let slot = *f.cache.index.get(&id).unwrap();
        // Tear the slot: flip a byte inside the framed block on the platter.
        f.cache
            .file
            .write_at(&[0xff; 4], (slot * SLOT_SIZE + super::super::HEADER_LEN) as u64)
            .unwrap();
        assert_eq!(read(&mut f.cache, &id), b"the true block", "the caller saw damage");
        // Counted as damage, and re-admitted on the same read, so a repeat
        // read now finds the freshly-written slot rather than the torn one.
        assert_eq!(f.cache.stats().corrupt_slots, 1);
        assert_eq!(read(&mut f.cache, &id), b"the true block");
    }

    #[test]
    fn a_slot_holding_the_wrong_block_is_rejected() {
        // A stale slot a torn write left behind must not be served as the block
        // the index names — identity is what catches it, not the checksum,
        // which the stale block passes on its own terms.
        let mut f = fixture(8);
        let real = f.cache.put(b"the wanted block", BlockType::SstData, 1).unwrap();
        let slot = *f.cache.index.get(&real).unwrap();
        // Overwrite the slot with a different, internally-valid framed block.
        let mut other = vec![0u8; SLOT_SIZE];
        let (_, n) = encode(b"a different block", BlockType::SstData, 2, &mut other).unwrap();
        f.cache.file.write_at(&other[..n], (slot * SLOT_SIZE) as u64).unwrap();
        // Read wants `real`; the slot holds something else, so it is a miss that
        // the store answers correctly.
        assert_eq!(read(&mut f.cache, &real), b"the wanted block");
        assert_eq!(f.cache.stats().corrupt_slots, 1);
    }

    #[test]
    fn a_short_buffer_is_refused_on_a_hit() {
        let mut f = fixture(8);
        let id = f.cache.put(b"0123456789", BlockType::SstData, 1).unwrap();
        let mut small = [0u8; 4];
        assert_eq!(f.cache.get(&id, &mut small).err(), Some(StoreError::BufferTooSmall));
    }

    #[test]
    fn slots_for_a_budget_is_whole_slots() {
        assert_eq!(DiskCache::<MemoryBlockStore>::slots_for(0), 0);
        assert_eq!(DiskCache::<MemoryBlockStore>::slots_for(SLOT_SIZE - 1), 0);
        assert_eq!(DiskCache::<MemoryBlockStore>::slots_for(SLOT_SIZE * 4 + 9), 4);
    }
}
