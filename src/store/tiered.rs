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

use super::BlockStore;
use super::cache::BlockCache;
use super::disk::{DiskCache, DiskError};

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
        TierSizing {
            bytes,
            units: BlockCache::<super::object::OwnedObjectStore>::frames_for(bytes),
        }
    }

    fn disk(bytes: usize) -> Self {
        TierSizing {
            bytes,
            units: DiskCache::<super::object::OwnedObjectStore>::slots_for(bytes),
        }
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
    pub(crate) fn ram_budget_bytes(&self) -> usize {
        let ram = if self.ram.units == 0 {
            0
        } else {
            BlockCache::<super::object::OwnedObjectStore>::budget_bytes(self.ram.units)
        };
        let disk = if self.disk.units == 0 {
            0
        } else {
            DiskCache::<super::object::OwnedObjectStore>::budget_bytes(self.disk.units)
        };
        ram + disk
    }

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
        Layer::Disk(DiskCache::open(
            budget,
            "disk cache",
            base,
            &path,
            plan.disk.units,
        )?)
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

impl<S: BlockStore> Layer<S> {
    fn base_mut(&mut self) -> &mut S {
        match self {
            Self::Base(store) => store,
            Self::Disk(cache) => cache.inner_mut(),
        }
    }
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

    fn get(
        &mut self,
        id: &super::BlockId,
        into: &mut [u8],
    ) -> Result<(usize, super::BlockType), super::StoreError> {
        match self {
            Layer::Base(s) => s.get(id, into),
            Layer::Disk(d) => d.get(id, into),
        }
    }

    fn get_packed(
        &mut self,
        container: &super::BlockId,
        offset: usize,
        length: usize,
        expected: &super::BlockId,
        into: &mut [u8],
        scratch: &mut [u8],
    ) -> Result<(usize, super::BlockType), super::StoreError> {
        match self {
            Layer::Base(store) => {
                store.get_packed(container, offset, length, expected, into, scratch)
            }
            Layer::Disk(cache) => {
                cache.get_packed(container, offset, length, expected, into, scratch)
            }
        }
    }

    #[cfg(test)]
    fn contains(&mut self, id: &super::BlockId) -> Result<bool, super::StoreError> {
        match self {
            Layer::Base(store) => store.contains(id),
            Layer::Disk(cache) => cache.contains(id),
        }
    }

    fn set_read_owner(&mut self, owner: Option<super::ReadOwner>) {
        match self {
            Layer::Base(store) => store.set_read_owner(owner),
            Layer::Disk(cache) => cache.set_read_owner(owner),
        }
    }

    fn cancel_read_owner(&mut self, owner: super::ReadOwner) {
        match self {
            Layer::Base(store) => store.cancel_read_owner(owner),
            Layer::Disk(cache) => cache.cancel_read_owner(owner),
        }
    }

    fn release_read(&mut self, id: &super::BlockId) {
        match self {
            Layer::Base(store) => store.release_read(id),
            Layer::Disk(cache) => cache.release_read(id),
        }
    }

    fn read_capacity_generation(&self) -> u64 {
        match self {
            Layer::Base(store) => store.read_capacity_generation(),
            Layer::Disk(cache) => cache.read_capacity_generation(),
        }
    }

    fn enable_async_gets(&mut self) {
        match self {
            Layer::Base(store) => store.enable_async_gets(),
            Layer::Disk(cache) => cache.enable_async_gets(),
        }
    }

    fn disable_async_gets(&mut self) {
        match self {
            Layer::Base(store) => store.disable_async_gets(),
            Layer::Disk(cache) => cache.disable_async_gets(),
        }
    }

    fn async_gets_enabled(&self) -> bool {
        match self {
            Layer::Base(store) => store.async_gets_enabled(),
            Layer::Disk(cache) => cache.async_gets_enabled(),
        }
    }

    fn prefetch(&mut self, id: &super::BlockId) -> Result<super::PrefetchState, super::StoreError> {
        match self {
            Layer::Base(store) => store.prefetch(id),
            Layer::Disk(cache) => cache.prefetch(id),
        }
    }

    fn take_prefetch(
        &mut self,
        id: &super::BlockId,
        into: &mut [u8],
    ) -> Result<Option<(usize, super::BlockType)>, super::StoreError> {
        match self {
            Layer::Base(store) => store.take_prefetch(id, into),
            Layer::Disk(cache) => cache.take_prefetch(id, into),
        }
    }

    fn async_read_slots(&self) -> usize {
        match self {
            Layer::Base(store) => store.async_read_slots(),
            Layer::Disk(cache) => cache.async_read_slots(),
        }
    }

    fn async_reads_busy(&self) -> bool {
        match self {
            Layer::Base(store) => store.async_reads_busy(),
            Layer::Disk(cache) => cache.async_reads_busy(),
        }
    }

    fn pending_read_fd(&self, slot: usize) -> Option<std::os::fd::RawFd> {
        match self {
            Layer::Base(store) => store.pending_read_fd(slot),
            Layer::Disk(cache) => cache.pending_read_fd(slot),
        }
    }

    fn pending_read_wants_write(&self, slot: usize) -> bool {
        match self {
            Layer::Base(store) => store.pending_read_wants_write(slot),
            Layer::Disk(cache) => cache.pending_read_wants_write(slot),
        }
    }

    fn advance_pending_read(&mut self, slot: usize) -> Result<bool, super::StoreError> {
        match self {
            Layer::Base(store) => store.advance_pending_read(slot),
            Layer::Disk(cache) => cache.advance_pending_read(slot),
        }
    }

    fn next_read_deadline(&self) -> Option<std::time::Instant> {
        match self {
            Layer::Base(store) => store.next_read_deadline(),
            Layer::Disk(cache) => cache.next_read_deadline(),
        }
    }

    fn advance_read_deadlines(&mut self, now: std::time::Instant) -> bool {
        match self {
            Layer::Base(store) => store.advance_read_deadlines(now),
            Layer::Disk(cache) => cache.advance_read_deadlines(now),
        }
    }

    fn io_stats(&self) -> super::BlockIoStats {
        match self {
            Layer::Base(store) => store.io_stats(),
            Layer::Disk(cache) => cache.io_stats(),
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

impl<S: BlockStore> TieredStore<S> {
    /// Maintenance uses its own network request on a miss. A cache hit must
    /// not release or attach to a statement's pending object read.
    pub(crate) fn get_cached(
        &mut self,
        id: &super::BlockId,
        into: &mut [u8],
    ) -> Result<Option<(usize, super::BlockType)>, super::StoreError> {
        let layer = match self {
            Self::WithRam(cache) => {
                if let Some(block) = cache.get_cached(id, into)? {
                    return Ok(Some(block));
                }
                cache.inner_mut()
            }
            Self::WithoutRam(layer) => layer,
        };
        match layer {
            Layer::Base(_) => Ok(None),
            Layer::Disk(cache) => cache.get_cached(id, into),
        }
    }

    /// Admit an already verified remote response without writing it again.
    pub(crate) fn cache_verified(
        &mut self,
        id: super::BlockId,
        kind: super::BlockType,
        payload: &[u8],
    ) {
        let layer = match self {
            Self::WithRam(cache) => {
                cache.admit(id, kind, payload);
                cache.inner_mut()
            }
            Self::WithoutRam(layer) => layer,
        };
        if let Layer::Disk(cache) = layer {
            cache.admit(id, kind, 0, payload);
        }
    }

    pub(crate) fn base_mut(&mut self) -> &mut S {
        match self {
            Self::WithRam(cache) => cache.inner_mut().base_mut(),
            Self::WithoutRam(layer) => layer.base_mut(),
        }
    }
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

    fn get(
        &mut self,
        id: &super::BlockId,
        into: &mut [u8],
    ) -> Result<(usize, super::BlockType), super::StoreError> {
        match self {
            TieredStore::WithRam(c) => c.get(id, into),
            TieredStore::WithoutRam(l) => l.get(id, into),
        }
    }

    fn get_packed(
        &mut self,
        container: &super::BlockId,
        offset: usize,
        length: usize,
        expected: &super::BlockId,
        into: &mut [u8],
        scratch: &mut [u8],
    ) -> Result<(usize, super::BlockType), super::StoreError> {
        match self {
            TieredStore::WithRam(cache) => {
                cache.get_packed(container, offset, length, expected, into, scratch)
            }
            TieredStore::WithoutRam(layer) => {
                layer.get_packed(container, offset, length, expected, into, scratch)
            }
        }
    }

    #[cfg(test)]
    fn contains(&mut self, id: &super::BlockId) -> Result<bool, super::StoreError> {
        match self {
            TieredStore::WithRam(cache) => cache.contains(id),
            TieredStore::WithoutRam(layer) => layer.contains(id),
        }
    }

    fn set_read_owner(&mut self, owner: Option<super::ReadOwner>) {
        match self {
            TieredStore::WithRam(cache) => cache.set_read_owner(owner),
            TieredStore::WithoutRam(layer) => layer.set_read_owner(owner),
        }
    }

    fn cancel_read_owner(&mut self, owner: super::ReadOwner) {
        match self {
            TieredStore::WithRam(cache) => cache.cancel_read_owner(owner),
            TieredStore::WithoutRam(layer) => layer.cancel_read_owner(owner),
        }
    }

    fn release_read(&mut self, id: &super::BlockId) {
        match self {
            TieredStore::WithRam(cache) => cache.release_read(id),
            TieredStore::WithoutRam(layer) => layer.release_read(id),
        }
    }

    fn read_capacity_generation(&self) -> u64 {
        match self {
            TieredStore::WithRam(cache) => cache.read_capacity_generation(),
            TieredStore::WithoutRam(layer) => layer.read_capacity_generation(),
        }
    }

    fn enable_async_gets(&mut self) {
        match self {
            TieredStore::WithRam(cache) => cache.enable_async_gets(),
            TieredStore::WithoutRam(layer) => layer.enable_async_gets(),
        }
    }

    fn disable_async_gets(&mut self) {
        match self {
            TieredStore::WithRam(cache) => cache.disable_async_gets(),
            TieredStore::WithoutRam(layer) => layer.disable_async_gets(),
        }
    }

    fn async_gets_enabled(&self) -> bool {
        match self {
            TieredStore::WithRam(cache) => cache.async_gets_enabled(),
            TieredStore::WithoutRam(layer) => layer.async_gets_enabled(),
        }
    }

    fn prefetch(&mut self, id: &super::BlockId) -> Result<super::PrefetchState, super::StoreError> {
        match self {
            TieredStore::WithRam(cache) => cache.prefetch(id),
            TieredStore::WithoutRam(layer) => layer.prefetch(id),
        }
    }

    fn take_prefetch(
        &mut self,
        id: &super::BlockId,
        into: &mut [u8],
    ) -> Result<Option<(usize, super::BlockType)>, super::StoreError> {
        match self {
            TieredStore::WithRam(cache) => cache.take_prefetch(id, into),
            TieredStore::WithoutRam(layer) => layer.take_prefetch(id, into),
        }
    }

    fn async_read_slots(&self) -> usize {
        match self {
            TieredStore::WithRam(cache) => cache.async_read_slots(),
            TieredStore::WithoutRam(layer) => layer.async_read_slots(),
        }
    }

    fn async_reads_busy(&self) -> bool {
        match self {
            TieredStore::WithRam(cache) => cache.async_reads_busy(),
            TieredStore::WithoutRam(layer) => layer.async_reads_busy(),
        }
    }

    fn pending_read_fd(&self, slot: usize) -> Option<std::os::fd::RawFd> {
        match self {
            TieredStore::WithRam(cache) => cache.pending_read_fd(slot),
            TieredStore::WithoutRam(layer) => layer.pending_read_fd(slot),
        }
    }

    fn pending_read_wants_write(&self, slot: usize) -> bool {
        match self {
            TieredStore::WithRam(cache) => cache.pending_read_wants_write(slot),
            TieredStore::WithoutRam(layer) => layer.pending_read_wants_write(slot),
        }
    }

    fn advance_pending_read(&mut self, slot: usize) -> Result<bool, super::StoreError> {
        match self {
            TieredStore::WithRam(cache) => cache.advance_pending_read(slot),
            TieredStore::WithoutRam(layer) => layer.advance_pending_read(slot),
        }
    }

    fn next_read_deadline(&self) -> Option<std::time::Instant> {
        match self {
            TieredStore::WithRam(cache) => cache.next_read_deadline(),
            TieredStore::WithoutRam(layer) => layer.next_read_deadline(),
        }
    }

    fn advance_read_deadlines(&mut self, now: std::time::Instant) -> bool {
        match self {
            TieredStore::WithRam(cache) => cache.advance_read_deadlines(now),
            TieredStore::WithoutRam(layer) => layer.advance_read_deadlines(now),
        }
    }

    fn io_stats(&self) -> super::BlockIoStats {
        match self {
            TieredStore::WithRam(cache) => cache.io_stats(),
            TieredStore::WithoutRam(layer) => layer.io_stats(),
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
        let (n, _) = s.get(id, &mut out).expect("reads");
        out.truncate(n);
        out
    }

    #[test]
    fn maintenance_metadata_cache_probes_never_consume_foreground_read_owners() {
        struct PendingStore {
            releases: usize,
            reads: usize,
        }
        impl BlockStore for PendingStore {
            fn put(
                &mut self,
                _: &[u8],
                _: BlockType,
                _: u64,
            ) -> Result<BlockId, super::super::StoreError> {
                panic!("verified cache admission must not PUT")
            }
            fn get(
                &mut self,
                _: &BlockId,
                _: &mut [u8],
            ) -> Result<(usize, BlockType), super::super::StoreError> {
                self.reads += 1;
                Err(super::super::StoreError::NotReady)
            }
            fn release_read(&mut self, _: &BlockId) {
                self.releases += 1;
            }
        }
        let payload = b"verified roster bytes";
        let id = BlockId::for_block(BlockType::SstRoster, payload);
        for (ram, disk) in [(FRAME, 0), (0, SLOT), (FRAME, SLOT), (0, 0)] {
            let path = dir();
            let plan = StackPlan::resolve(ram, disk);
            let mut budget = Budget::new(plan.ram_budget_bytes());
            let mut tiers = build(
                &mut budget,
                PendingStore {
                    releases: 0,
                    reads: 0,
                },
                plan,
                &path,
            )
            .unwrap();
            let mut into = [0u8; 64];
            crate::mem::guard::forbid_alloc(|| {
                assert_eq!(tiers.get_cached(&id, &mut into).unwrap(), None);
                tiers.cache_verified(id, BlockType::SstRoster, payload);
                // Re-admission after a foreground hit must not duplicate an identity.
                tiers.cache_verified(id, BlockType::SstRoster, payload);
                let cached = tiers.get_cached(&id, &mut into).unwrap();
                assert_eq!(cached.is_some(), ram != 0 || disk != 0);
                if cached.is_some() {
                    assert_eq!(&into[..payload.len()], payload);
                }
                assert_eq!(tiers.base_mut().reads, 0);
                assert_eq!(tiers.base_mut().releases, 0);
                let _ = tiers.get(&id, &mut into);
                assert_eq!(
                    tiers.base_mut().releases,
                    usize::from(ram != 0 || disk != 0)
                );
                assert_eq!(tiers.base_mut().reads, usize::from(ram == 0 && disk == 0));
            });
            drop(tiers);
            std::fs::remove_dir_all(path).unwrap();
        }
    }

    #[test]
    fn startup_budget_covers_cache_payloads_metadata_and_disk_scratch() {
        for ram in [0, FRAME, 3 * FRAME + 17] {
            for disk in [0, SLOT, 3 * SLOT + 19] {
                let (_, store) = base();
                let path = dir();
                let plan = StackPlan::resolve(ram, disk);
                let mut budget = Budget::new(plan.ram_budget_bytes());
                let stack = build(&mut budget, store, plan, &path).unwrap();
                assert_eq!(budget.remaining(), 0);
                drop(stack);
                std::fs::remove_dir_all(path).unwrap();
            }
        }
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
        assert!(
            !plan.undersized_ram(),
            "zero means the tier was not asked for"
        );
        assert!(!plan.undersized_disk());
    }

    #[test]
    fn a_full_stack_serves_reads_and_writes() {
        let (mut budget, store) = base();
        let plan = StackPlan::resolve(FRAME * 4, SLOT * 8);
        let mut stack = build(&mut budget, store, plan, &dir()).expect("builds");
        assert!(matches!(stack, TieredStore::WithRam(_)));
        let id = stack
            .put(b"through every tier", BlockType::SstData, 1)
            .unwrap();
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
        let id = stack
            .put(b"straight to store", BlockType::SstData, 1)
            .unwrap();
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
        assert_eq!(
            stack.get(&absent, &mut out).err(),
            Some(super::super::StoreError::NotFound)
        );
    }
}
