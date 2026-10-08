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
        self.state.read().expect("row heap lock poisoned").buffer.len()
    }

    pub(super) fn exclusive(&mut self) -> &mut RowHeapState {
        self.state.get_mut().expect("row heap lock poisoned")
    }

    #[cfg(test)]
    pub(super) fn test_write(&self) -> std::sync::TryLockResult<std::sync::RwLockWriteGuard<'_, RowHeapState>> {
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
        let location = RowLoc { offset: self.used as u32, len: len as u32 };
        let slice = &mut self.buffer[self.used..self.used + len];
        self.used += len;
        Ok((location, slice))
    }

    pub(super) fn validate(&self, location: RowLoc) -> Result<(), SqlError> {
        let end = (location.offset as usize).checked_add(location.len as usize);
        if end.is_none_or(|end| end > self.used) {
            return Err(sql_err!(sqlstate::INTERNAL_ERROR, "row location exceeds initialized heap bytes"));
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
                    return Err(sql_err!(sqlstate::INTERNAL_ERROR, "heap compaction contains overlapping row locations"));
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
            self.buffer.copy_within(start..start + source.len as usize, destination);
        }
    }

    pub(super) fn finish_relocation(&mut self, used: usize) {
        debug_assert!(used <= self.used);
        self.used = used;
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
            assert!(read.other(RowLoc { offset: 3, len: 2 }).is_err());
            drop(read);
            assert!(heap.test_write().is_ok());
            assert!(heap.get(RowLoc { offset: u32::MAX, len: u32::MAX }).is_err());
            assert!(heap.get(RowLoc { offset: 4, len: 1 }).is_err());
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
            assert_eq!(empty, RowLoc { offset: 4, len: 0 });
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
            let overlapping = [RowLoc { offset: 0, len: 5 }, RowLoc { offset: 4, len: 2 }];
            assert_eq!(state.validate_relocation(overlapping.into_iter()).unwrap_err().sqlstate, sqlstate::INTERNAL_ERROR);
            let outside = [RowLoc { offset: 8, len: 5 }];
            assert!(state.validate_relocation(outside.into_iter()).is_err());
            assert_eq!(&*heap.get(RowLoc { offset: 0, len: 12 }).unwrap(), b"abcdefghijkl");
            assert_eq!(heap.used(), 12);
            let state = heap.exclusive();
            let valid = [RowLoc { offset: 4, len: 4 }, RowLoc { offset: 4, len: 4 }, RowLoc { offset: 4, len: 0 }, RowLoc { offset: 8, len: 4 }];
            state.validate_relocation(valid.into_iter()).unwrap();
            state.relocate(valid[0], 0);
            state.relocate(valid[3], 4);
            state.finish_relocation(8);
            assert_eq!(&*heap.get(RowLoc { offset: 0, len: 8 }).unwrap(), b"efghijkl");
            assert!(heap.get(RowLoc { offset: 8, len: 1 }).is_err());
        });
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
        assert!(crate::config::Config::parse("memtable_bytes = 4GiB").unwrap_err().message.contains("row-location"));
    }
}
