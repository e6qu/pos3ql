//! Assembling the read path: RAM frames over a disk file over a base store.
//!
//! Stages A and B built each tier on its own. This is where they stack into the
//! read path the founding vision named — memory and disk as a cache in front of
//! object storage — and where `block_cache_bytes` and `disk_cache_bytes` stop
//! being config fields wired to nothing: each sizes the tier it names, and a
//! zero for either drops that tier from the stack rather than building an empty
//! one that would miss on every read.
//!
//! The base store is a type parameter, so the same stack sits over the object
//! backend in the running server and over the memory backend under test. What
//! the assembled stack presents is still a [`BlockStore`]: a caller reads and
//! writes blocks and never learns how many tiers answered.

use std::path::Path;

use crate::mem::budget::Budget;

use super::cache::BlockCache;
use super::disk::{DiskCache, DiskError};
use super::BlockStore;

/// One tier's byte budget, and how many whole units of that tier it buys. A
/// budget smaller than one unit buys nothing and is reported as such, so the
/// caller can drop the tier rather than build a cache that cannot hold a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TierSizing {
    pub(crate) bytes: usize,
    pub(crate) units: usize,
}

impl TierSizing {
    fn ram(bytes: usize) -> Self {
        TierSizing { bytes, units: BlockCache::<super::memory::MemoryBlockStore>::frames_for(bytes) }
    }

    fn disk(bytes: usize) -> Self {
        TierSizing { bytes, units: DiskCache::<super::memory::MemoryBlockStore>::slots_for(bytes) }
    }
}

/// What a byte budget resolves to before anything is built. Returned so a caller
/// can print the plan and refuse a misconfiguration before reserving memory —
/// `block_cache_bytes` set to less than one frame is almost certainly a typo,
/// not a request for a one-tier stack, and saying so beats silently obliging.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StackPlan {
    pub(crate) ram: TierSizing,
    pub(crate) disk: TierSizing,
}

impl StackPlan {
    pub(crate) fn resolve(block_cache_bytes: usize, disk_cache_bytes: usize) -> Self {
        StackPlan {
            ram: TierSizing::ram(block_cache_bytes),
            disk: TierSizing::disk(disk_cache_bytes),
        }
    }

    /// A tier asked for but too small to hold a block. Neither is fatal on its
    /// own — a stack can run on fewer tiers — but it is a configuration the
    /// caller should see rather than have silently swallowed.
    pub(crate) fn undersized_ram(&self) -> bool {
        self.ram.bytes > 0 && self.ram.units == 0
    }

    pub(crate) fn undersized_disk(&self) -> bool {
        self.disk.bytes > 0 && self.disk.units == 0
    }
}

/// The read path over `base`, with whichever tiers the plan sized to at least
/// one unit. A tier asked for but too small is dropped, not built empty; both
/// dropped leaves the base store answering directly, which is the RAM-only
/// database the earlier phases already were.
///
/// The disk tier needs a file, so `cache_dir` is where its slot file lives; it
/// is unused when the disk tier is absent.
pub(crate) fn build<S: BlockStore>(
    budget: &mut Budget,
    base: S,
    plan: StackPlan,
    cache_dir: &Path,
) -> Result<TieredStore<S>, DiskError> {
    let over_disk = if plan.disk.units > 0 {
        let path = cache_dir.join("block-cache");
        Layer::Disk(DiskCache::open(budget, "disk cache", base, &path, plan.disk.units)?)
    } else {
        Layer::Base(base)
    };
    if plan.ram.units > 0 {
        let cache = BlockCache::new(budget, "block cache", over_disk, plan.ram.units)
            .map_err(DiskError::Budget)?;
        Ok(TieredStore::WithRam(cache))
    } else {
        Ok(TieredStore::WithoutRam(over_disk))
    }
}

/// Either the base store or a disk cache in front of it — the part of the stack
/// beneath the optional RAM tier. Kept as an enum rather than a boxed trait so
/// no allocation and no dynamic dispatch enter the read path.
pub(crate) enum Layer<S: BlockStore> {
    Base(S),
    Disk(DiskCache<S>),
}

impl<S: BlockStore> BlockStore for Layer<S> {
    fn put(
        &mut self,
        payload: &[u8],
        block_type: super::BlockType,
        lsn: u64,
    ) -> Result<super::BlockId, super::StoreError> {
        match self {
            Layer::Base(s) => s.put(payload, block_type, lsn),
            Layer::Disk(d) => d.put(payload, block_type, lsn),
        }
    }

    fn get(&mut self, id: &super::BlockId, into: &mut [u8]) -> Result<usize, super::StoreError> {
        match self {
            Layer::Base(s) => s.get(id, into),
            Layer::Disk(d) => d.get(id, into),
        }
    }

    fn contains(&mut self, id: &super::BlockId) -> Result<bool, super::StoreError> {
        match self {
            Layer::Base(s) => s.contains(id),
            Layer::Disk(d) => d.contains(id),
        }
    }
}

/// The assembled read path. Which tiers it has is fixed at build time, so the
/// variants stand for the two shapes a stack can take rather than a per-read
/// choice.
pub(crate) enum TieredStore<S: BlockStore> {
    WithRam(BlockCache<Layer<S>>),
    WithoutRam(Layer<S>),
}

impl<S: BlockStore> BlockStore for TieredStore<S> {
    fn put(
        &mut self,
        payload: &[u8],
        block_type: super::BlockType,
        lsn: u64,
    ) -> Result<super::BlockId, super::StoreError> {
        match self {
            TieredStore::WithRam(c) => c.put(payload, block_type, lsn),
            TieredStore::WithoutRam(l) => l.put(payload, block_type, lsn),
        }
    }

    fn get(&mut self, id: &super::BlockId, into: &mut [u8]) -> Result<usize, super::StoreError> {
        match self {
            TieredStore::WithRam(c) => c.get(id, into),
            TieredStore::WithoutRam(l) => l.get(id, into),
        }
    }

    fn contains(&mut self, id: &super::BlockId) -> Result<bool, super::StoreError> {
        match self {
            TieredStore::WithRam(c) => c.contains(id),
            TieredStore::WithoutRam(l) => l.contains(id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::MemoryBlockStore;
    use crate::store::{BlockId, BlockType};

    const FRAME: usize = super::super::MAX_PAYLOAD;
    const SLOT: usize = super::super::BLOCK_SIZE;

    fn base() -> (Budget, MemoryBlockStore) {
        let mut budget = Budget::new(64 << 20);
        let store = MemoryBlockStore::new(&mut budget, "base", 8 << 20, 512).expect("base fits");
        (budget, store)
    }

    fn dir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("pos3ql-tiered-{n}"));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    fn read(s: &mut TieredStore<MemoryBlockStore>, id: &BlockId) -> Vec<u8> {
        let mut out = vec![0u8; 4096];
        let n = s.get(id, &mut out).expect("reads");
        out.truncate(n);
        out
    }

    #[test]
    fn the_config_knobs_size_the_tiers() {
        let plan = StackPlan::resolve(FRAME * 4, SLOT * 10);
        assert_eq!(plan.ram.units, 4);
        assert_eq!(plan.disk.units, 10);
        assert!(!plan.undersized_ram() && !plan.undersized_disk());
    }

    #[test]
    fn a_tier_smaller_than_a_unit_is_reported_undersized() {
        let plan = StackPlan::resolve(FRAME - 1, SLOT - 1);
        assert_eq!(plan.ram.units, 0);
        assert_eq!(plan.disk.units, 0);
        assert!(plan.undersized_ram());
        assert!(plan.undersized_disk());
    }

    #[test]
    fn zero_is_not_undersized_it_is_absent() {
        let plan = StackPlan::resolve(0, 0);
        assert!(!plan.undersized_ram(), "zero means the tier was not asked for");
        assert!(!plan.undersized_disk());
    }

    #[test]
    fn a_full_stack_serves_reads_and_writes() {
        let (mut budget, store) = base();
        let plan = StackPlan::resolve(FRAME * 4, SLOT * 8);
        let mut stack = build(&mut budget, store, plan, &dir()).expect("builds");
        assert!(matches!(stack, TieredStore::WithRam(_)));
        let id = stack.put(b"through every tier", BlockType::SstData, 1).unwrap();
        assert_eq!(read(&mut stack, &id), b"through every tier");
        assert!(stack.contains(&id).unwrap());
    }

    #[test]
    fn a_zero_ram_budget_builds_a_disk_only_stack() {
        let (mut budget, store) = base();
        let plan = StackPlan::resolve(0, SLOT * 8);
        let mut stack = build(&mut budget, store, plan, &dir()).expect("builds");
        assert!(matches!(stack, TieredStore::WithoutRam(Layer::Disk(_))));
        let id = stack.put(b"disk only", BlockType::SstData, 1).unwrap();
        assert_eq!(read(&mut stack, &id), b"disk only");
    }

    #[test]
    fn both_zero_leaves_the_base_store_answering_directly() {
        // The RAM-only database the earlier phases were: no cache in front, the
        // store reached on every read.
        let (mut budget, store) = base();
        let plan = StackPlan::resolve(0, 0);
        let mut stack = build(&mut budget, store, plan, &dir()).expect("builds");
        assert!(matches!(stack, TieredStore::WithoutRam(Layer::Base(_))));
        let id = stack.put(b"straight to store", BlockType::SstData, 1).unwrap();
        assert_eq!(read(&mut stack, &id), b"straight to store");
    }

    #[test]
    fn a_block_written_once_is_read_back_through_the_whole_stack() {
        // Every tier writes through, so the same block is present at each and a
        // read is a RAM hit — the fast path the stack exists to provide.
        let (mut budget, store) = base();
        let plan = StackPlan::resolve(FRAME * 8, SLOT * 16);
        let mut stack = build(&mut budget, store, plan, &dir()).expect("builds");
        let ids: Vec<_> = (0..5u8)
            .map(|i| stack.put(&[i; 300], BlockType::SstData, i as u64).unwrap())
            .collect();
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(read(&mut stack, id), vec![i as u8; 300], "block {i}");
        }
    }

    #[test]
    fn a_missing_block_is_absent_at_every_tier() {
        let (mut budget, store) = base();
        let plan = StackPlan::resolve(FRAME * 4, SLOT * 8);
        let mut stack = build(&mut budget, store, plan, &dir()).expect("builds");
        let absent = BlockId::of(b"never written");
        assert!(!stack.contains(&absent).unwrap());
        let mut out = [0u8; 64];
        assert_eq!(stack.get(&absent, &mut out).err(), Some(super::super::StoreError::NotFound));
    }
}
