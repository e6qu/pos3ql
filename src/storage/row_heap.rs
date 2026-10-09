//! Fixed row bytes and append position share one ownership boundary.

use core::mem::size_of;
use core::ops::Deref;
use std::sync::{RwLock, RwLockReadGuard};

use super::RowLoc;
use crate::mem::budget::{Budget, BudgetError};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

pub(crate) struct RowHeap {
    state: RwLock<RowHeapState>,
}

pub(super) struct RowHeapState {
    buffer: Box<[u8]>,
    used: usize,
    generation: u64,
}

pub(crate) struct HeapRowRead<'a> {
    state: RwLockReadGuard<'a, RowHeapState>,
    location: RowLoc,
}

impl Deref for HeapRowRead<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        let start = self.location.offset as usize;
        &self.state.buffer[start..start + self.location.len as usize]
    }
}

impl HeapRowRead<'_> {
    pub(crate) fn other(&self, location: RowLoc) -> Result<&[u8], SqlError> {
        self.state.validate(location)?;
        let start = location.offset as usize;
        Ok(&self.state.buffer[start..start + location.len as usize])
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
                buffer: vec![0; bytes].into_boxed_slice(),
                used: 0,
                generation: 1,
            }),
        })
    }

    pub(crate) fn append(&mut self, len: usize) -> Result<(RowLoc, &mut [u8]), SqlError> {
        self.exclusive().append(len)
    }

    pub(crate) fn get(&self, location: RowLoc) -> Result<HeapRowRead<'_>, SqlError> {
        let state = self.state.read().expect("row heap lock poisoned");
        state.validate(location)?;
        Ok(HeapRowRead { state, location })
    }

    pub(crate) fn used(&self) -> usize {
        self.state.read().expect("row heap lock poisoned").used
    }

    pub(crate) fn capacity(&self) -> usize {
        self.state
            .read()
            .expect("row heap lock poisoned")
            .buffer
            .len()
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
    fn append(&mut self, len: usize) -> Result<(RowLoc, &mut [u8]), SqlError> {
        if len > self.buffer.len() - self.used {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "memtable is full ({} bytes); with object storage on, rows spill at the next checkpoint — retry, raise memtable_bytes, or enable object storage",
                self.buffer.len()
            ));
        }
        let location = RowLoc {
            offset: self.used as u32,
            len: len as u32,
            generation: self.generation,
        };
        let slice = &mut self.buffer[self.used..self.used + len];
        self.used += len;
        Ok((location, slice))
    }

    pub(super) fn validate(&self, location: RowLoc) -> Result<(), SqlError> {
        if location.generation != self.generation {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "row heap location was invalidated by relocation"
            ));
        }
        let end = (location.offset as usize).checked_add(location.len as usize);
        if end.is_none_or(|end| end > self.used) {
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
                .copy_within(start..start + source.len as usize, destination);
        }
    }

    pub(super) fn next_relocation_generation(&self) -> Result<u64, SqlError> {
        self.generation.checked_add(1).ok_or_else(|| sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "row heap relocation generation is exhausted"
        ))
    }

    pub(super) fn finish_relocation(&mut self, used: usize, generation: u64) {
        debug_assert!(used <= self.used);
        debug_assert_eq!(self.generation.checked_add(1), Some(generation));
        self.used = used;
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
            assert!(
                heap.get(RowLoc::test(u32::MAX, u32::MAX))
                .is_err()
            );
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
            assert_eq!(
                &*heap.get(RowLoc::test(0, 12)).unwrap(),
                b"abcdefghijkl"
            );
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
                &*heap.get(RowLoc { offset: 0, len: 8, generation }).unwrap(),
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
            let current = RowLoc { offset: 0, len: 4, generation };
            let (new, bytes) = heap.append(4).unwrap();
            bytes.copy_from_slice(b"next");
            assert_eq!(old.offset, new.offset);
            let error = match heap.get(old) {
                Ok(_) => panic!("a reused range must reject its stale locator"),
                Err(error) => error,
            };
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            assert_eq!(error.message.as_str(), "row heap location was invalidated by relocation");
            let read = heap.get(current).unwrap();
            assert_eq!(&*read, b"keep");
            assert_eq!(read.other(new).unwrap(), b"next");
            assert_eq!(read.other(old).unwrap_err().sqlstate, sqlstate::SERIALIZATION_FAILURE);
            assert_eq!(read.other(RowLoc::EMPTY).unwrap_err().sqlstate, sqlstate::SERIALIZATION_FAILURE);
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
            state.validate_relocation(core::iter::once(location)).unwrap();
            let error = state.next_relocation_generation().unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(error.message.as_str(), "row heap relocation generation is exhausted");
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
                                Err(error) => assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE),
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
                            writer.buffer[..8].copy_from_slice(&epoch.to_le_bytes());
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
