//! Published row bytes are immutable until exclusive relocation; appends use disjoint tails.

use core::cell::UnsafeCell;
use core::mem::size_of;
use core::ops::Deref;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, RwLock, RwLockReadGuard};

use super::RowLoc;
use crate::mem::budget::{Budget, BudgetError};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

pub(crate) struct RowHeap {
    state: RwLock<RowHeapState>,
}

pub(super) struct RowHeapState {
    buffer: Box<UnsafeCell<[u8]>>,
    capacity: usize,
    used: AtomicUsize,
    append: Mutex<()>,
    generation: u64,
}

// SAFETY: readers access only ranges below the acquired publication frontier.
// One append owner initializes the disjoint tail before releasing that frontier.
// Every append and reader holds a relocation read guard; relocation and the
// exclusive fixture API require exclusive state ownership before changing bytes.
unsafe impl Sync for RowHeapState {}

pub(crate) struct HeapRowRead<'a> {
    state: RwLockReadGuard<'a, RowHeapState>,
    location: RowLoc,
}

impl Deref for HeapRowRead<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        let start = self.location.offset as usize;
        self.state
            .published_slice(start, self.location.len as usize)
    }
}

impl HeapRowRead<'_> {
    pub(crate) fn location(&self) -> RowLoc {
        self.location
    }

    pub(crate) fn other(&self, location: RowLoc) -> Result<&[u8], SqlError> {
        self.state.validate(location)?;
        let start = location.offset as usize;
        Ok(self.state.published_slice(start, location.len as usize))
    }
}

impl RowHeap {
    pub(super) const fn control_bytes() -> usize {
        size_of::<Self>()
    }

    pub(super) fn new(budget: &mut Budget, bytes: usize) -> Result<Self, BudgetError> {
        if bytes > u32::MAX as usize {
            return Err(BudgetError {
                what: "row heap address range",
                requested: bytes,
                remaining: budget.remaining(),
                total: budget.total(),
            });
        }
        budget.draw(Self::control_bytes(), "row heap controls")?;
        budget.draw(bytes, "memtable")?;
        Ok(Self {
            state: RwLock::new(RowHeapState {
                // SAFETY: UnsafeCell is layout-transparent and ownership of the
                // allocation transfers exactly once; its length is unchanged.
                buffer: unsafe {
                    Box::from_raw(
                        Box::into_raw(vec![0u8; bytes].into_boxed_slice()) as *mut UnsafeCell<[u8]>
                    )
                },
                capacity: bytes,
                used: AtomicUsize::new(0),
                append: Mutex::new(()),
                generation: 1,
            }),
        })
    }

    #[cfg(test)]
    pub(crate) fn append(&mut self, len: usize) -> Result<(RowLoc, &mut [u8]), SqlError> {
        self.exclusive().append(len)
    }

    pub(crate) fn append_bytes(&self, bytes: &[u8]) -> Result<RowLoc, SqlError> {
        self.append_with(bytes.len(), |out| {
            out.copy_from_slice(bytes);
            Ok(())
        })
    }

    pub(crate) fn append_row(
        &self,
        values: &[crate::sql::types::Datum<'_>],
    ) -> Result<RowLoc, SqlError> {
        self.append_with(super::rowenc::encoded_len(values), |out| {
            super::rowenc::encode(values, out);
            Ok(())
        })
    }

    /// Initialization cannot retain the writable range or publish its location.
    /// Release byte ownership before taking row-version or catalog write owners.
    fn append_with(
        &self,
        len: usize,
        initialize: impl FnOnce(&mut [u8]) -> Result<(), SqlError>,
    ) -> Result<RowLoc, SqlError> {
        let state = self.state.read().expect("row heap lock poisoned");
        let _append = state.append.lock().expect("row heap append lock poisoned");
        let start = state.used.load(Ordering::Acquire);
        state.check_capacity(start, len)?;
        let location = RowLoc {
            offset: start as u32,
            len: len as u32,
            generation: state.generation,
        };
        // SAFETY: append ownership excludes another initializer. This tail is
        // beyond the publication frontier and cannot overlap any issued read.
        // The relocation guard prevents movement or reuse until initialization
        // finishes. No reference to this range escapes the initializer.
        let out = unsafe {
            core::slice::from_raw_parts_mut(state.buffer.get().cast::<u8>().add(start), len)
        };
        initialize(out)?;
        state.used.store(start + len, Ordering::Release);
        Ok(location)
    }

    pub(crate) fn get(&self, location: RowLoc) -> Result<HeapRowRead<'_>, SqlError> {
        let state = self.state.read().expect("row heap lock poisoned");
        state.validate(location)?;
        Ok(HeapRowRead { state, location })
    }

    pub(crate) fn used(&self) -> usize {
        self.state
            .read()
            .expect("row heap lock poisoned")
            .used
            .load(Ordering::Acquire)
    }

    pub(crate) fn capacity(&self) -> usize {
        self.state.read().expect("row heap lock poisoned").capacity
    }

    pub(super) fn exclusive(&mut self) -> &mut RowHeapState {
        self.state.get_mut().expect("row heap lock poisoned")
    }

    #[cfg(test)]
    pub(super) fn test_write(
        &self,
    ) -> std::sync::TryLockResult<std::sync::RwLockWriteGuard<'_, RowHeapState>> {
        self.state.try_write()
    }
}

impl RowHeapState {
    fn published_slice(&self, start: usize, len: usize) -> &[u8] {
        // SAFETY: callers validated this range against an acquired frontier
        // under a relocation guard. Published bytes are never appended over.
        unsafe { core::slice::from_raw_parts(self.buffer.get().cast::<u8>().add(start), len) }
    }

    fn check_capacity(&self, start: usize, len: usize) -> Result<(), SqlError> {
        if len > self.capacity - start {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "memtable is full ({} bytes); with object storage on, rows spill at the next checkpoint — retry, raise memtable_bytes, or enable object storage",
                self.capacity
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    fn append(&mut self, len: usize) -> Result<(RowLoc, &mut [u8]), SqlError> {
        let start = *self.used.get_mut();
        self.check_capacity(start, len)?;
        let location = RowLoc {
            offset: start as u32,
            len: len as u32,
            generation: self.generation,
        };
        *self.used.get_mut() = start + len;
        Ok((location, &mut self.buffer.get_mut()[start..start + len]))
    }

    pub(super) fn validate(&self, location: RowLoc) -> Result<(), SqlError> {
        if location.generation != self.generation {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "row heap location was invalidated by relocation"
            ));
        }
        let end = (location.offset as usize).checked_add(location.len as usize);
        if end.is_none_or(|end| end > self.used.load(Ordering::Acquire)) {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "row location exceeds initialized heap bytes"
            ));
        }
        Ok(())
    }

    /// Preflight the complete relocation set before moving any bytes or handles.
    pub(super) fn validate_relocation(
        &self,
        locations: impl Iterator<Item = RowLoc>,
    ) -> Result<(), SqlError> {
        let mut prior: Option<RowLoc> = None;
        for location in locations {
            self.validate(location)?;
            if location.len == 0 {
                continue;
            }
            if let Some(previous) = prior {
                if previous == location {
                    continue;
                }
                if (location.offset as usize) < previous.offset as usize + previous.len as usize {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "heap compaction contains overlapping row locations"
                    ));
                }
            }
            prior = Some(location);
        }
        Ok(())
    }

    pub(super) fn relocate(&mut self, source: RowLoc, destination: usize) {
        let start = source.offset as usize;
        debug_assert!(destination <= start);
        if start != destination {
            self.buffer
                .get_mut()
                .copy_within(start..start + source.len as usize, destination);
        }
    }

    pub(super) fn next_relocation_generation(&self) -> Result<u64, SqlError> {
        self.generation.checked_add(1).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "row heap relocation generation is exhausted"
            )
        })
    }

    pub(super) fn finish_relocation(&mut self, used: usize, generation: u64) {
        debug_assert!(used <= *self.used.get_mut());
        debug_assert_eq!(self.generation.checked_add(1), Some(generation));
        *self.used.get_mut() = used;
        self.generation = generation;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heap(bytes: usize) -> RowHeap {
        let mut budget = Budget::new(bytes + RowHeap::control_bytes());
        let heap = RowHeap::new(&mut budget, bytes).unwrap();
        assert_eq!(budget.remaining(), 0);
        heap
    }

    #[test]
    fn shared_heap_append_preserves_frontier_on_failure_and_exhaustion() {
        let heap = heap(8);
        crate::mem::guard::forbid_alloc(|| {
            let old = heap.append_bytes(b"keep").unwrap();
            let pinned = heap.get(old).unwrap();
            let error = heap
                .append_with(4, |out| {
                    out[..2].copy_from_slice(b"xx");
                    Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "encoding failed"
                    ))
                })
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(heap.used(), 4);
            assert!(pinned.other(RowLoc::test(4, 4)).is_err());
            assert_eq!(&*pinned, b"keep");
            for len in [5, usize::MAX] {
                assert_eq!(
                    heap.append_with(len, |_| panic!("capacity must preflight"))
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::PROGRAM_LIMIT_EXCEEDED
                );
                assert_eq!(heap.used(), 4);
            }
            let new = heap.append_bytes(b"next").unwrap();
            assert_eq!(new, RowLoc::test(4, 4));
            assert_eq!(pinned.other(new).unwrap(), b"next");
            assert_eq!(
                heap.append_bytes(b"!").unwrap_err().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
        });
    }

    #[test]
    fn shared_heap_append_completes_while_an_older_reader_is_pinned() {
        use std::sync::atomic::AtomicBool;
        let heap = heap(8);
        let old = heap.append_bytes(b"keep").unwrap();
        let pinned = heap.get(old).unwrap();
        let done = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                crate::mem::guard::forbid_alloc(|| {
                    let location = heap.append_bytes(b"next").unwrap();
                    done.store(true, Ordering::Release);
                    location
                })
            });
            let completed_while_pinned = crate::mem::guard::forbid_alloc(|| {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while !done.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
                    assert_eq!(&*pinned, b"keep");
                    std::thread::yield_now();
                }
                done.load(Ordering::Acquire)
            });
            drop(pinned);
            let new = writer.join().unwrap();
            assert!(
                completed_while_pinned,
                "append must progress before the old reader releases"
            );
            assert_eq!(&*heap.get(new).unwrap(), b"next");
        });
    }

    #[test]
    fn shared_heap_append_hides_incomplete_bytes_from_existing_readers() {
        use std::sync::atomic::AtomicBool;
        let heap = heap(8);
        let old = heap.append_bytes(b"keep").unwrap();
        let pinned = heap.get(old).unwrap();
        let entered = AtomicBool::new(false);
        let release = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                crate::mem::guard::forbid_alloc(|| {
                    heap.append_with(4, |out| {
                        out[..2].copy_from_slice(b"ne");
                        entered.store(true, Ordering::Release);
                        while !release.load(Ordering::Acquire) {
                            std::thread::yield_now();
                        }
                        out[2..].copy_from_slice(b"xt");
                        Ok(())
                    })
                    .unwrap()
                })
            });
            let initialized_while_pinned = crate::mem::guard::forbid_alloc(|| {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while !entered.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
                    std::thread::yield_now();
                }
                let entered = entered.load(Ordering::Acquire);
                assert_eq!(&*pinned, b"keep");
                assert!(pinned.other(RowLoc::test(4, 4)).is_err());
                release.store(true, Ordering::Release);
                entered
            });
            drop(pinned);
            let new = writer.join().unwrap();
            assert!(initialized_while_pinned);
            assert_eq!(&*heap.get(new).unwrap(), b"next");
        });
    }

    #[test]
    fn shared_heap_append_parallel_writers_publish_disjoint_complete_ranges() {
        let heap = heap(128 * 8);
        let start = std::sync::Barrier::new(5);
        std::thread::scope(|scope| {
            let writers: [_; 4] = core::array::from_fn(|worker| {
                let heap = &heap;
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    crate::mem::guard::forbid_alloc(|| {
                        let mut locations = [RowLoc::EMPTY; 32];
                        for (index, location) in locations.iter_mut().enumerate() {
                            let value = ((worker as u64) << 32) | index as u64;
                            *location = heap.append_bytes(&value.to_le_bytes()).unwrap();
                            assert_eq!(&*heap.get(*location).unwrap(), &value.to_le_bytes());
                        }
                        locations
                    })
                })
            });
            start.wait();
            let mut occupied = [false; 128];
            for (worker, writer) in writers.into_iter().enumerate() {
                for (index, location) in writer.join().unwrap().into_iter().enumerate() {
                    let slot = location.offset as usize / 8;
                    assert!(!occupied[slot]);
                    occupied[slot] = true;
                    let value = ((worker as u64) << 32) | index as u64;
                    assert_eq!(&*heap.get(location).unwrap(), &value.to_le_bytes());
                }
            }
            assert!(occupied.into_iter().all(|used| used));
            assert_eq!(heap.used(), heap.capacity());
        });
    }

    #[test]
    fn row_heap_ownership_pins_bytes_and_releases_after_errors() {
        let mut heap = heap(8);
        let (location, bytes) = heap.append(4).unwrap();
        bytes.copy_from_slice(b"abcd");
        crate::mem::guard::forbid_alloc(|| {
            let read = heap.get(location).unwrap();
            assert!(heap.test_write().is_err());
            assert_eq!(&*read, b"abcd");
            assert_eq!(read.other(location).unwrap(), b"abcd");
            assert!(read.other(RowLoc::test(3, 2)).is_err());
            drop(read);
            assert!(heap.test_write().is_ok());
            assert!(heap.get(RowLoc::test(u32::MAX, u32::MAX)).is_err());
            assert!(heap.get(RowLoc::test(4, 1)).is_err());
            assert!(heap.test_write().is_ok());
            assert_eq!(heap.used(), 4);
            assert_eq!(heap.capacity(), 8);
        });
    }

    #[test]
    fn row_heap_ownership_exhaustion_preserves_position_and_initialized_bytes() {
        let mut heap = heap(4);
        crate::mem::guard::forbid_alloc(|| {
            let (location, bytes) = heap.append(4).unwrap();
            bytes.copy_from_slice(b"full");
            for len in [1, usize::MAX] {
                let error = heap.append(len).unwrap_err();
                assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
                assert_eq!(heap.used(), 4);
                assert_eq!(&*heap.get(location).unwrap(), b"full");
            }
            let (empty, bytes) = heap.append(0).unwrap();
            assert!(bytes.is_empty());
            assert_eq!(empty, RowLoc::test(4, 0));
            assert!(heap.get(empty).unwrap().is_empty());
        });
    }

    #[test]
    fn row_heap_ownership_relocation_rejects_invalid_sets_before_mutation() {
        let mut heap = heap(12);
        let (_, bytes) = heap.append(12).unwrap();
        bytes.copy_from_slice(b"abcdefghijkl");
        crate::mem::guard::forbid_alloc(|| {
            let state = heap.exclusive();
            let overlapping = [RowLoc::test(0, 5), RowLoc::test(4, 2)];
            assert_eq!(
                state
                    .validate_relocation(overlapping.into_iter())
                    .unwrap_err()
                    .sqlstate,
                sqlstate::INTERNAL_ERROR
            );
            let outside = [RowLoc::test(8, 5)];
            assert!(state.validate_relocation(outside.into_iter()).is_err());
            assert_eq!(&*heap.get(RowLoc::test(0, 12)).unwrap(), b"abcdefghijkl");
            assert_eq!(heap.used(), 12);
            let state = heap.exclusive();
            let valid = [
                RowLoc::test(4, 4),
                RowLoc::test(4, 4),
                RowLoc::test(4, 0),
                RowLoc::test(8, 4),
            ];
            state.validate_relocation(valid.into_iter()).unwrap();
            state.relocate(valid[0], 0);
            state.relocate(valid[3], 4);
            let generation = state.next_relocation_generation().unwrap();
            state.finish_relocation(8, generation);
            assert_eq!(
                &*heap
                    .get(RowLoc {
                        offset: 0,
                        len: 8,
                        generation
                    })
                    .unwrap(),
                b"efghijkl"
            );
            assert!(heap.get(RowLoc::test(8, 1)).is_err());
        });
    }

    #[test]
    fn row_heap_generation_rejects_reused_ranges_and_preserves_current_appends() {
        let mut heap = heap(12);
        let (_, bytes) = heap.append(4).unwrap();
        bytes.copy_from_slice(b"dead");
        let (old, bytes) = heap.append(4).unwrap();
        bytes.copy_from_slice(b"keep");
        crate::mem::guard::forbid_alloc(|| {
            let state = heap.exclusive();
            state.validate_relocation(core::iter::once(old)).unwrap();
            let generation = state.next_relocation_generation().unwrap();
            state.relocate(old, 0);
            state.finish_relocation(4, generation);
            let current = RowLoc {
                offset: 0,
                len: 4,
                generation,
            };
            let (new, bytes) = heap.append(4).unwrap();
            bytes.copy_from_slice(b"next");
            assert_eq!(old.offset, new.offset);
            let error = match heap.get(old) {
                Ok(_) => panic!("a reused range must reject its stale locator"),
                Err(error) => error,
            };
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            assert_eq!(
                error.message.as_str(),
                "row heap location was invalidated by relocation"
            );
            let read = heap.get(current).unwrap();
            assert_eq!(&*read, b"keep");
            assert_eq!(read.other(new).unwrap(), b"next");
            assert_eq!(
                read.other(old).unwrap_err().sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            assert_eq!(
                read.other(RowLoc::EMPTY).unwrap_err().sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            drop(read);
            assert!(heap.test_write().is_ok());
        });
    }

    #[test]
    fn row_heap_generation_exhaustion_preserves_bytes_and_current_locations() {
        let mut heap = heap(4);
        heap.exclusive().generation = u64::MAX;
        let (location, bytes) = heap.append(4).unwrap();
        bytes.copy_from_slice(b"last");
        crate::mem::guard::forbid_alloc(|| {
            let state = heap.exclusive();
            state
                .validate_relocation(core::iter::once(location))
                .unwrap();
            let error = state.next_relocation_generation().unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(
                error.message.as_str(),
                "row heap relocation generation is exhausted"
            );
            assert_eq!(&*heap.get(location).unwrap(), b"last");
            assert_eq!(heap.used(), 4);
        });
    }

    #[test]
    fn row_heap_generation_readers_reject_detached_locations_during_relocation() {
        let mut heap = heap(8);
        let (_, bytes) = heap.append(4).unwrap();
        bytes.copy_from_slice(b"dead");
        let (old, bytes) = heap.append(4).unwrap();
        bytes.copy_from_slice(b"keep");
        let start = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            for _ in 0..3 {
                let heap = &heap;
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    crate::mem::guard::forbid_alloc(|| {
                        for _ in 0..500 {
                            match heap.get(old) {
                                Ok(read) => assert_eq!(&*read, b"keep"),
                                Err(error) => {
                                    assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE)
                                }
                            }
                        }
                    });
                });
            }
            start.wait();
            crate::mem::guard::forbid_alloc(|| {
                let mut state = heap.state.write().unwrap();
                state.validate_relocation(core::iter::once(old)).unwrap();
                let generation = state.next_relocation_generation().unwrap();
                state.relocate(old, 0);
                state.finish_relocation(4, generation);
                let (_, bytes) = state.append(4).unwrap();
                bytes.copy_from_slice(b"next");
            });
        });
        assert!(heap.get(old).is_err());
        assert!(heap.test_write().is_ok());
    }

    #[test]
    fn row_heap_ownership_readers_observe_complete_byte_updates() {
        let mut heap = heap(8);
        let (location, bytes) = heap.append(8).unwrap();
        bytes.copy_from_slice(&0u64.to_le_bytes());
        let start = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            for _ in 0..3 {
                let heap = &heap;
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    crate::mem::guard::forbid_alloc(|| {
                        for _ in 0..500 {
                            let row = heap.get(location).unwrap();
                            let epoch = u64::from_le_bytes((&*row).try_into().unwrap());
                            assert!(epoch <= 500);
                            assert_eq!(row.other(location).unwrap(), &epoch.to_le_bytes());
                        }
                    });
                });
            }
            start.wait();
            crate::mem::guard::forbid_alloc(|| {
                for epoch in 1u64..=500 {
                    loop {
                        if let Ok(mut writer) = heap.test_write() {
                            writer.buffer.get_mut()[..8].copy_from_slice(&epoch.to_le_bytes());
                            break;
                        }
                        std::thread::yield_now();
                    }
                }
            });
        });
        assert_eq!(&*heap.get(location).unwrap(), &500u64.to_le_bytes());
    }

    #[test]
    fn row_heap_ownership_controls_and_address_limits_are_charged_at_startup() {
        let _ = heap(8);
        let mut short = Budget::new(8 + RowHeap::control_bytes() - 1);
        assert!(RowHeap::new(&mut short, 8).is_err());
        if usize::BITS > 32 {
            let mut budget = Budget::new(usize::MAX);
            let error = match RowHeap::new(&mut budget, u32::MAX as usize + 1) {
                Ok(_) => panic!("unaddressable heap accepted"),
                Err(error) => error,
            };
            assert_eq!(error.what, "row heap address range");
            assert_eq!(budget.used(), 0);
        }
        assert!(
            crate::config::Config::parse("memtable_bytes = 4GiB")
                .unwrap_err()
                .message
                .contains("row-location")
        );
    }
}
