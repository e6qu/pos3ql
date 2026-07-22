//! The RAM tier: a fixed set of frames in front of a slower block store.
//!
//! This is where `block_cache_bytes` finally means something. A read asks the
//! cache first and only reaches the store it wraps on a miss, so a working set
//! that fits here is served without a round trip while one that does not still
//! answers — slower, from whatever the cache sits in front of.
//!
//! Eviction is CLOCK, as TigerBeetle's grid cache has it: one referenced bit
//! per frame and a hand that sweeps, clearing bits until it meets a frame that
//! has not been touched since the last pass. It approximates least-recently-used
//! closely enough for this and costs one bit and one pointer, where true LRU
//! costs a list whose links must be maintained on every hit.
//!
//! Being a cache rather than a store is what lets it evict at all: every block
//! here is re-fetchable from the layer behind, so dropping one loses nothing but
//! time. That is also why a frame is never dirty — writes go through to the
//! store first, and the cache only remembers what the store already accepted.

use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_map::FixedMap;

use super::{BlockId, BlockStore, BlockType, StoreError, MAX_PAYLOAD};

/// What a frame is holding, if anything.
#[derive(Clone, Copy)]
struct Frame {
    id: Option<BlockId>,
    len: usize,
    /// Touched since the hand last passed. CLOCK clears it rather than evicting
    /// on the first sweep, so a block used twice survives one round of pressure.
    referenced: bool,
}

/// How the cache has been doing. Surfaced rather than kept internal: a cache
/// whose hit ratio cannot be seen is a cache nobody can size.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct CacheStats {
    pub(crate) hits: u64,
    pub(crate) misses: u64,
    pub(crate) evictions: u64,
    /// Blocks admitted on a write-through rather than on a miss.
    pub(crate) insertions: u64,
}

pub(crate) struct BlockCache<S: BlockStore> {
    inner: S,
    /// One slot per frame, each `MAX_PAYLOAD` wide. Payloads are cached rather
    /// than framed blocks: the block was verified on the way in, so keeping the
    /// header would be storing what has already been checked.
    slots: Box<[u8]>,
    frames: Box<[Frame]>,
    /// Identity to frame index, so a hit is a lookup rather than a scan.
    index: FixedMap<BlockId, usize>,
    /// The CLOCK hand.
    hand: usize,
    stats: CacheStats,
}

impl<S: BlockStore> BlockCache<S> {
    /// Reserves `frame_count` frames. The budget is drawn once here, so the
    /// cache's size is decided at startup and never afterwards.
    pub(crate) fn new(
        budget: &mut Budget,
        what: &'static str,
        inner: S,
        frame_count: usize,
    ) -> Result<Self, BudgetError> {
        assert!(frame_count > 0, "a cache with no frames would miss on every read");
        budget.draw_array(frame_count, MAX_PAYLOAD, what)?;
        budget.draw_array(frame_count, size_of::<Frame>(), what)?;
        let index = FixedMap::new(budget, what, frame_count)?;
        Ok(Self {
            inner,
            slots: vec![0u8; frame_count * MAX_PAYLOAD].into_boxed_slice(),
            frames: vec![Frame { id: None, len: 0, referenced: false }; frame_count]
                .into_boxed_slice(),
            index,
            hand: 0,
            stats: CacheStats::default(),
        })
    }

    pub(crate) fn stats(&self) -> CacheStats {
        self.stats
    }

    /// How many frames a byte budget buys. Zero bytes buys no cache, which the
    /// constructor refuses — a caller that wants none should not build one.
    pub(crate) fn frames_for(bytes: usize) -> usize {
        bytes / MAX_PAYLOAD
    }


    /// Whether the block is resident in a frame, as distinct from reachable
    /// through the store behind. Only the tests care about the difference —
    /// every caller should see a cache as an accelerator, not a directory.
    #[cfg(test)]
    fn contains_in_cache(&self, id: &BlockId) -> bool {
        self.index.get(id).is_some()
    }

    fn slot(&self, frame: usize) -> &[u8] {
        &self.slots[frame * MAX_PAYLOAD..(frame + 1) * MAX_PAYLOAD]
    }

    /// Chooses a frame, evicting the one the hand settles on. The sweep is
    /// bounded: every frame's bit is cleared in at most one full pass, so a
    /// second pass must find one to take.
    fn claim_frame(&mut self) -> usize {
        loop {
            let at = self.hand;
            self.hand = (self.hand + 1) % self.frames.len();
            if self.frames[at].referenced {
                self.frames[at].referenced = false;
                continue;
            }
            if let Some(evicted) = self.frames[at].id.take() {
                self.index.remove(&evicted);
                self.stats.evictions += 1;
            }
            return at;
        }
    }

    fn admit(&mut self, id: BlockId, payload: &[u8]) {
        // A payload too large for a frame is simply not cached; it still
        // reached the caller from the store, so nothing is lost but the reuse.
        if payload.len() > MAX_PAYLOAD {
            return;
        }
        let frame = self.claim_frame();
        self.slots[frame * MAX_PAYLOAD..frame * MAX_PAYLOAD + payload.len()]
            .copy_from_slice(payload);
        self.frames[frame] = Frame { id: Some(id), len: payload.len(), referenced: true };
        // The index has one slot per frame and the frame was just freed, so
        // this cannot overflow; a failure would mean the two disagree.
        self.index.insert(id, frame).expect("index has a slot per frame");
    }
}

impl<S: BlockStore> BlockStore for BlockCache<S> {
    /// Writes through, then remembers. The store is what makes a block durable,
    /// so it decides first; caching a block the store rejected would serve reads
    /// of something that was never written.
    fn put(
        &mut self,
        payload: &[u8],
        block_type: BlockType,
        lsn: u64,
    ) -> Result<BlockId, StoreError> {
        let id = self.inner.put(payload, block_type, lsn)?;
        if self.index.get(&id).is_none() {
            self.admit(id, payload);
            self.stats.insertions += 1;
        }
        Ok(id)
    }

    fn get(&mut self, id: &BlockId, into: &mut [u8]) -> Result<usize, StoreError> {
        if let Some(&frame) = self.index.get(id) {
            let len = self.frames[frame].len;
            if into.len() < len {
                return Err(StoreError::BufferTooSmall);
            }
            into[..len].copy_from_slice(&self.slot(frame)[..len]);
            self.frames[frame].referenced = true;
            self.stats.hits += 1;
            return Ok(len);
        }
        self.stats.misses += 1;
        let len = self.inner.get(id, into)?;
        // Admitted from the caller's buffer, which now holds exactly the block
        // the store returned and verified.
        self.admit(*id, &into[..len]);
        Ok(len)
    }

    /// Presence in the cache proves presence in the store — a frame is only
    /// filled from a block the store accepted or returned — so a hit answers
    /// without a round trip. A miss has to ask, because the cache holds a
    /// subset and not knowing a block is not knowing it is absent.
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

    fn cache(frames: usize) -> BlockCache<MemoryBlockStore> {
        let mut budget = Budget::new((frames + 4) * MAX_PAYLOAD + (16 << 20));
        let inner = MemoryBlockStore::new(&mut budget, "test store", 4 << 20, 256)
            .expect("store fits");
        BlockCache::new(&mut budget, "test cache", inner, frames).expect("cache fits")
    }

    fn read(c: &mut BlockCache<MemoryBlockStore>, id: &BlockId) -> Vec<u8> {
        let mut out = vec![0u8; 4096];
        let n = c.get(id, &mut out).expect("reads");
        out.truncate(n);
        out
    }

    #[test]
    fn a_written_block_is_served_without_reaching_the_store() {
        let mut c = cache(4);
        let id = c.put(b"written through", BlockType::SstData, 1).unwrap();
        assert_eq!(c.stats().insertions, 1);
        assert_eq!(read(&mut c, &id), b"written through");
        assert_eq!(c.stats().hits, 1);
        assert_eq!(c.stats().misses, 0);
    }

    #[test]
    fn a_miss_fetches_then_serves_from_the_cache() {
        let mut c = cache(4);
        // Written straight to the store, so the cache has never seen it.
        let id = c.inner.put(b"cold block", BlockType::SstData, 1).unwrap();
        assert_eq!(read(&mut c, &id), b"cold block");
        assert_eq!(c.stats().misses, 1);
        assert_eq!(read(&mut c, &id), b"cold block");
        assert_eq!(c.stats().hits, 1, "the second read was served from the frame");
    }

    #[test]
    fn eviction_loses_nothing_because_the_store_still_has_it() {
        // The property that makes a cache safe to evict from: every block here
        // is re-fetchable, so pressure costs time and never data.
        let mut c = cache(2);
        let ids: Vec<_> = (0..6u8)
            .map(|i| c.put(&[i; 64], BlockType::SstData, i as u64).unwrap())
            .collect();
        assert!(c.stats().evictions > 0, "six blocks through two frames must evict");
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(read(&mut c, id), vec![i as u8; 64], "block {i} was lost");
        }
    }

    #[test]
    fn a_reused_block_outlives_an_untouched_neighbour() {
        // CLOCK's point: the referenced bit buys a block one reprieve. It only
        // shows when some frames are unreferenced — with every bit set the hand
        // clears them all and takes the first, which is FIFO and correct. So
        // the bits are cleared by a sweep first, then one block is touched, and
        // the next eviction must fall on a neighbour rather than on it.
        let mut c = cache(3);
        let a = c.put(b"aaaa", BlockType::SstData, 0).unwrap();
        let b = c.put(b"bbbb", BlockType::SstData, 1).unwrap();
        let d = c.put(b"dddd", BlockType::SstData, 2).unwrap();
        // One more admission sweeps the hand across all three, clearing them,
        // and evicts `a` — leaving `b` and `d` resident with their bits down.
        c.put(b"eeee", BlockType::SstData, 3).unwrap();
        assert!(!c.contains_in_cache(&a), "the sweep should have taken the oldest");
        // Touch `b`, so the hand must pass over it and take `d` instead.
        assert_eq!(read(&mut c, &b), b"bbbb");
        c.put(b"ffff", BlockType::SstData, 4).unwrap();
        assert!(c.contains_in_cache(&b), "a touched block was evicted before an untouched one");
        assert!(!c.contains_in_cache(&d), "the untouched neighbour should have gone");
    }

    #[test]
    fn the_cache_never_answers_for_a_block_the_store_rejected() {
        // A store at its limit must not leave the cache serving a block that
        // was never stored.
        let mut budget = Budget::new(64 * MAX_PAYLOAD);
        let inner = MemoryBlockStore::new(&mut budget, "tiny store", 1024, 8).expect("fits");
        let mut c = BlockCache::new(&mut budget, "cache", inner, 4).expect("fits");
        let mut stored = 0;
        for i in 0..32u8 {
            if c.put(&[i; 256], BlockType::SstData, i as u64).is_ok() {
                stored += 1;
            } else {
                let id = BlockId::of(&[i; 256]);
                assert!(!c.contains(&id).unwrap(), "cached a block the store refused");
                break;
            }
        }
        assert!(stored > 0, "nothing was stored at all");
    }

    #[test]
    fn a_short_buffer_is_refused_on_a_hit_as_well_as_a_miss() {
        let mut c = cache(4);
        let id = c.put(b"0123456789", BlockType::SstData, 1).unwrap();
        let mut small = [0u8; 4];
        assert_eq!(c.get(&id, &mut small).err(), Some(StoreError::BufferTooSmall));
        // And again once it is definitely a hit.
        assert_eq!(c.get(&id, &mut small).err(), Some(StoreError::BufferTooSmall));
    }

    #[test]
    fn contains_answers_from_a_frame_and_falls_through_otherwise() {
        let mut c = cache(4);
        let cached = c.put(b"in a frame", BlockType::SstData, 1).unwrap();
        let uncached = c.inner.put(b"store only", BlockType::SstData, 2).unwrap();
        assert!(c.contains(&cached).unwrap());
        assert!(c.contains(&uncached).unwrap(), "a miss must ask the store");
        assert!(!c.contains(&BlockId::of(b"nowhere")).unwrap());
    }

    #[test]
    fn frames_for_a_budget_is_whole_frames() {
        assert_eq!(BlockCache::<MemoryBlockStore>::frames_for(0), 0);
        assert_eq!(BlockCache::<MemoryBlockStore>::frames_for(MAX_PAYLOAD - 1), 0);
        assert_eq!(BlockCache::<MemoryBlockStore>::frames_for(MAX_PAYLOAD), 1);
        assert_eq!(BlockCache::<MemoryBlockStore>::frames_for(MAX_PAYLOAD * 3 + 7), 3);
    }
}
