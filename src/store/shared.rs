//! Borrow a shared store for each operation; reader callbacks own no store guard.

use std::os::fd::RawFd;
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

use super::{BlockId, BlockIoStats, BlockStore, BlockType, PrefetchState, StoreError};

pub(crate) struct SharedBlockStore<'store, Store: ?Sized> {
    inner: &'store Mutex<Store>,
}

impl<'store, Store: ?Sized> SharedBlockStore<'store, Store> {
    pub(crate) fn new(inner: &'store Mutex<Store>) -> Self {
        Self { inner }
    }

    fn lock(&self) -> MutexGuard<'_, Store> {
        self.inner.lock().expect("block store lock poisoned")
    }
}

impl<Store: BlockStore + ?Sized> BlockStore for SharedBlockStore<'_, Store> {
    fn put(&mut self, payload: &[u8], kind: BlockType, lsn: u64) -> Result<BlockId, StoreError> {
        self.lock().put(payload, kind, lsn)
    }

    fn get(&mut self, id: &BlockId, into: &mut [u8]) -> Result<(usize, BlockType), StoreError> {
        self.lock().get(id, into)
    }

    #[cfg(test)]
    fn contains(&mut self, id: &BlockId) -> Result<bool, StoreError> {
        self.lock().contains(id)
    }

    fn get_packed(
        &mut self,
        container: &BlockId,
        offset: usize,
        length: usize,
        expected: &BlockId,
        into: &mut [u8],
        scratch: &mut [u8],
    ) -> Result<(usize, BlockType), StoreError> {
        self.lock()
            .get_packed(container, offset, length, expected, into, scratch)
    }

    fn set_read_owner(&mut self, owner: Option<super::ReadOwner>) {
        self.lock().set_read_owner(owner);
    }

    fn cancel_read_owner(&mut self, owner: super::ReadOwner) {
        self.lock().cancel_read_owner(owner);
    }

    fn release_read(&mut self, id: &BlockId) {
        self.lock().release_read(id);
    }

    fn read_capacity_generation(&self) -> u64 {
        self.lock().read_capacity_generation()
    }

    fn enable_async_gets(&mut self) {
        self.lock().enable_async_gets();
    }

    fn disable_async_gets(&mut self) {
        self.lock().disable_async_gets();
    }

    fn async_gets_enabled(&self) -> bool {
        self.lock().async_gets_enabled()
    }

    fn prefetch(&mut self, id: &BlockId) -> Result<PrefetchState, StoreError> {
        self.lock().prefetch(id)
    }

    fn take_prefetch(
        &mut self,
        id: &BlockId,
        into: &mut [u8],
    ) -> Result<Option<(usize, BlockType)>, StoreError> {
        self.lock().take_prefetch(id, into)
    }

    fn async_read_slots(&self) -> usize {
        self.lock().async_read_slots()
    }

    fn async_reads_busy(&self) -> bool {
        self.lock().async_reads_busy()
    }

    fn pending_read_fd(&self, slot: usize) -> Option<RawFd> {
        self.lock().pending_read_fd(slot)
    }

    fn pending_read_wants_write(&self, slot: usize) -> bool {
        self.lock().pending_read_wants_write(slot)
    }

    fn advance_pending_read(&mut self, slot: usize) -> Result<bool, StoreError> {
        self.lock().advance_pending_read(slot)
    }

    fn next_read_deadline(&self) -> Option<Instant> {
        self.lock().next_read_deadline()
    }

    fn advance_read_deadlines(&mut self, now: Instant) -> bool {
        self.lock().advance_read_deadlines(now)
    }

    fn io_stats(&self) -> BlockIoStats {
        self.lock().io_stats()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PendingPackedStore {
        enabled: bool,
        packed_reads: usize,
        advanced: bool,
        hedge: Instant,
        hedged: bool,
    }

    impl BlockStore for PendingPackedStore {
        fn put(&mut self, _: &[u8], _: BlockType, _: u64) -> Result<BlockId, StoreError> {
            Err(StoreError::Unavailable)
        }

        fn get(&mut self, _: &BlockId, _: &mut [u8]) -> Result<(usize, BlockType), StoreError> {
            panic!("a packed read must delegate the ranged operation")
        }

        fn get_packed(
            &mut self,
            container: &BlockId,
            offset: usize,
            length: usize,
            expected: &BlockId,
            into: &mut [u8],
            _: &mut [u8],
        ) -> Result<(usize, BlockType), StoreError> {
            assert_eq!(*container, BlockId([1; 32]));
            assert_eq!(*expected, BlockId([2; 32]));
            assert_eq!((offset, length), (17, 23));
            into[0] = 9;
            self.packed_reads += 1;
            Ok((1, BlockType::ValueIndexData))
        }

        fn enable_async_gets(&mut self) {
            self.enabled = true;
        }
        fn disable_async_gets(&mut self) {
            self.enabled = false;
        }
        fn async_gets_enabled(&self) -> bool {
            self.enabled
        }
        fn async_read_slots(&self) -> usize {
            3
        }
        fn async_reads_busy(&self) -> bool {
            !self.advanced
        }
        fn pending_read_fd(&self, slot: usize) -> Option<RawFd> {
            (slot == 2 && !self.advanced).then_some(42)
        }
        fn advance_pending_read(&mut self, slot: usize) -> Result<bool, StoreError> {
            assert_eq!(slot, 2);
            self.advanced = true;
            Ok(true)
        }
        fn prefetch(&mut self, _: &BlockId) -> Result<PrefetchState, StoreError> {
            Ok(PrefetchState::Reused)
        }
        fn take_prefetch(
            &mut self,
            _: &BlockId,
            into: &mut [u8],
        ) -> Result<Option<(usize, BlockType)>, StoreError> {
            into[0] = 7;
            Ok(Some((1, BlockType::ValueIndexData)))
        }
        fn next_read_deadline(&self) -> Option<Instant> {
            Some(self.hedge)
        }
        fn advance_read_deadlines(&mut self, now: Instant) -> bool {
            assert_eq!(now, self.hedge);
            self.hedged = true;
            true
        }
        fn io_stats(&self) -> BlockIoStats {
            BlockIoStats {
                object_gets: 11,
                ..BlockIoStats::default()
            }
        }
    }

    #[test]
    fn index_file_owners_shared_store_preserves_packed_and_pending_read_contracts() {
        let hedge = Instant::now();
        let store = Mutex::new(PendingPackedStore {
            enabled: false,
            packed_reads: 0,
            advanced: false,
            hedge,
            hedged: false,
        });
        crate::mem::guard::forbid_alloc(|| {
            let mut shared = SharedBlockStore::new(&store);
            shared.enable_async_gets();
            assert!(shared.async_gets_enabled());
            assert_eq!(shared.async_read_slots(), 3);
            assert!(shared.async_reads_busy());
            assert_eq!(shared.pending_read_fd(2), Some(42));
            assert_eq!(shared.next_read_deadline(), Some(hedge));
            assert!(shared.advance_read_deadlines(hedge));
            assert!(shared.advance_pending_read(2).unwrap());
            assert!(!shared.async_reads_busy());
            assert_eq!(shared.pending_read_fd(2), None);
            let mut into = [0; 1];
            assert_eq!(
                shared
                    .get_packed(
                        &BlockId([1; 32]),
                        17,
                        23,
                        &BlockId([2; 32]),
                        &mut into,
                        &mut [0; 1]
                    )
                    .unwrap(),
                (1, BlockType::ValueIndexData)
            );
            assert_eq!(into, [9]);
            assert_eq!(
                shared.prefetch(&BlockId([2; 32])).unwrap(),
                PrefetchState::Reused
            );
            assert_eq!(
                shared.take_prefetch(&BlockId([2; 32]), &mut into).unwrap(),
                Some((1, BlockType::ValueIndexData))
            );
            assert_eq!(into, [7]);
            assert_eq!(shared.io_stats().object_gets, 11);
            shared.disable_async_gets();
            assert!(!shared.async_gets_enabled());
            let inner = store.try_lock().expect("the wrapper retains no guard");
            assert_eq!(inner.packed_reads, 1);
            assert!(inner.hedged);
        });
    }
}
