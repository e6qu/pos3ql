//! Movable scan state uses checked handles instead of references into an arena.

use super::*;
use core::ops::{Deref, DerefMut};
use core::ptr::NonNull;

pub(crate) struct ArenaSliceHandle<T> {
    identity: u64,
    offset: usize,
    value: PhantomData<T>,
    invariant: PhantomData<fn(T) -> T>,
}

impl<T> Copy for ArenaSliceHandle<T> {}

impl<T> Clone for ArenaSliceHandle<T> {
    fn clone(&self) -> Self {
        *self
    }
}

pub(crate) struct FrozenArenaSliceHandle<T>(ArenaSliceHandle<T>);

impl<T> Copy for FrozenArenaSliceHandle<T> {}

impl<T> Clone for FrozenArenaSliceHandle<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> PartialEq for FrozenArenaSliceHandle<T> {
    fn eq(&self, other: &Self) -> bool {
        self.0.identity == other.0.identity && self.0.offset == other.0.offset
    }
}

impl<T> Eq for FrozenArenaSliceHandle<T> {}

struct SliceHeader {
    len: usize,
    borrowed: Cell<bool>,
    frozen: Cell<bool>,
}

fn slice_layout<T>(len: usize) -> Result<(Layout, usize), std::alloc::LayoutError> {
    Layout::new::<SliceHeader>().extend(Layout::array::<T>(len)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArenaSliceError {
    Stale,
    Borrowed,
    Frozen,
}

pub(crate) struct ArenaSliceMut<'arena, T> {
    data: NonNull<T>,
    len: usize,
    borrowed: &'arena Cell<bool>,
    value: PhantomData<&'arena mut [T]>,
}

impl<T> Drop for ArenaSliceMut<'_, T> {
    fn drop(&mut self) {
        self.borrowed.set(false);
    }
}

impl<T> Deref for ArenaSliceMut<'_, T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        // The handle's checked identity pins this allocation. Its header
        // admits exactly one guard until Drop, including during unwinding.
        unsafe { core::slice::from_raw_parts(self.data.as_ptr(), self.len) }
    }
}

impl<T> DerefMut for ArenaSliceMut<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // The per-allocation guard excludes every other slice of these bytes.
        unsafe { core::slice::from_raw_parts_mut(self.data.as_ptr(), self.len) }
    }
}

impl<T> AsMut<[T]> for ArenaSliceMut<'_, T> {
    fn as_mut(&mut self) -> &mut [T] {
        self
    }
}

impl Arena {
    /// Closes a child workspace and transfers its encoded output directly into
    /// the selected owner's immutable storage. Temporary query memory and the
    /// output owner may differ when an invocation has survived a scheduler wait.
    pub(crate) fn with_remaining_retained_bytes<E>(
        &self,
        target: &Arena,
        each: impl for<'region> FnOnce(&'region Arena) -> Result<&'region [u8], E>,
    ) -> Result<Result<FrozenArenaSliceHandle<u8>, E>, ArenaFull> {
        if target.retention_identity == 0 {
            return Err(target.full(usize::MAX));
        }
        let output = self.with_remaining_region(|region| {
            each(region).map(|bytes| (bytes.as_ptr(), bytes.len()))
        })?;
        let (source, len) = match output {
            Ok(output) => output,
            Err(error) => return Ok(Err(error)),
        };
        let (layout, data_offset) = slice_layout::<u8>(len).map_err(|_| target.full(len))?;
        let base = target.alloc_raw_tail(layout)?;
        // Ending the region releases its frontier, not the backing bytes.
        // Reservation does not initialize them. Move the payload before writing
        // its header: either destination may overlap the child's old output.
        unsafe {
            core::ptr::copy(source, base.add(data_offset), len);
            base.cast::<SliceHeader>().write(SliceHeader {
                len,
                borrowed: Cell::new(false),
                frozen: Cell::new(true),
            });
        }
        Ok(Ok(FrozenArenaSliceHandle(ArenaSliceHandle {
            identity: target.retention_identity,
            offset: base as usize - target.base as usize,
            value: PhantomData,
            invariant: PhantomData,
        })))
    }

    /// Upper bound including alignment when the preceding tail is unaligned.
    #[cfg(test)]
    pub(crate) fn retained_slice_capacity<T>(len: usize) -> Option<usize> {
        let (layout, _) = slice_layout::<T>(len).ok()?;
        layout.size().checked_add(layout.align() - 1)
    }

    pub(crate) fn retain_slice_with<T: Copy>(
        &self,
        len: usize,
        mut fill: impl FnMut(usize) -> T,
    ) -> Result<ArenaSliceHandle<T>, ArenaFull> {
        if self.retention_identity == 0 {
            return Err(self.full(usize::MAX));
        }
        let (layout, data_offset) = slice_layout::<T>(len).map_err(|_| self.full(usize::MAX))?;
        let base = self.alloc_raw_tail(layout)?;
        // These disjoint bytes belong to this allocation and are unpublished
        // until every Copy value has been initialized successfully.
        unsafe {
            base.cast::<SliceHeader>().write(SliceHeader {
                len,
                borrowed: Cell::new(false),
                frozen: Cell::new(false),
            });
            let data = base.add(data_offset).cast::<T>();
            for at in 0..len {
                data.add(at).write(fill(at));
            }
        }
        Ok(ArenaSliceHandle {
            identity: self.retention_identity,
            offset: base as usize - self.base as usize,
            value: PhantomData,
            invariant: PhantomData,
        })
    }

    pub(crate) fn retained_slice_mut<'arena, T: Copy + 'arena>(
        &'arena self,
        handle: ArenaSliceHandle<T>,
    ) -> Result<ArenaSliceMut<'arena, T>, ArenaSliceError> {
        let header = self.retained_slice_header(handle)?;
        // Matching non-reused identity proves this typed header and its whole
        // initialized payload are still in this arena's persistent tail.
        let base = unsafe { self.base.add(handle.offset) };
        if header.frozen.get() {
            return Err(ArenaSliceError::Frozen);
        }
        if header.borrowed.replace(true) {
            return Err(ArenaSliceError::Borrowed);
        }
        let data_offset =
            core::mem::size_of::<SliceHeader>().next_multiple_of(core::mem::align_of::<T>());
        Ok(ArenaSliceMut {
            data: unsafe { NonNull::new_unchecked(base.add(data_offset).cast::<T>()) },
            len: header.len,
            borrowed: &header.borrowed,
            value: PhantomData,
        })
    }

    fn retained_slice_header<T>(
        &self,
        handle: ArenaSliceHandle<T>,
    ) -> Result<&SliceHeader, ArenaSliceError> {
        if handle.identity != self.retention_identity {
            return Err(ArenaSliceError::Stale);
        }
        // A matching, non-reused identity proves the typed allocation still exists.
        Ok(unsafe { &*self.base.add(handle.offset).cast::<SliceHeader>() })
    }

    pub(crate) fn freeze_retained_slice<T: Copy>(
        &self,
        handle: ArenaSliceHandle<T>,
    ) -> Result<FrozenArenaSliceHandle<T>, ArenaSliceError> {
        let header = self.retained_slice_header(handle)?;
        if header.borrowed.get() {
            return Err(ArenaSliceError::Borrowed);
        }
        if header.frozen.replace(true) {
            return Err(ArenaSliceError::Frozen);
        }
        Ok(FrozenArenaSliceHandle(handle))
    }

    pub(crate) fn retained_slice<'arena, T: Copy + 'arena>(
        &'arena self,
        handle: FrozenArenaSliceHandle<T>,
    ) -> Result<&'arena [T], ArenaSliceError> {
        let header = self.retained_slice_header(handle.0)?;
        // Freezing is irreversible. All mutable aliases check the same header
        // before exposing payload bytes, so these shared views cannot be changed.
        assert!(header.frozen.get());
        let data_offset =
            core::mem::size_of::<SliceHeader>().next_multiple_of(core::mem::align_of::<T>());
        let data = unsafe { self.base.add(handle.0.offset + data_offset).cast::<T>() };
        Ok(unsafe { core::slice::from_raw_parts(data, header.len) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_region_transfers_keep_explicit_parent_outputs_after_query_teardown() {
        let mut budget = Budget::new(4096);
        let owner = Arena::new(&mut budget, "explicit retained output", 4096).unwrap();
        let image = crate::mem::guard::forbid_alloc(|| {
            owner
                .with_remaining_region(|query| {
                    query
                        .with_remaining_retained_bytes(&owner, |scratch| {
                            scratch
                                .alloc_slice_with(300, |at| (at * 37 % 251) as u8)
                                .map(|bytes| &*bytes)
                        })
                        .unwrap()
                        .unwrap()
                })
                .unwrap()
        });
        let mut moved = owner;
        crate::mem::guard::forbid_alloc(|| {
            moved.recycle_front();
            moved.alloc_slice_with(1024, |_| 0xccu8).unwrap();
            let bytes = moved.retained_slice(image).unwrap();
            assert_eq!(bytes.len(), 300);
            for (at, byte) in bytes.iter().enumerate() {
                assert_eq!(*byte, (at * 37 % 251) as u8);
            }
            moved.reset();
            assert!(moved.retained_slice(image).is_err());
        });
    }

    #[test]
    fn retained_region_transfers_move_overlapping_payload_before_initializing_headers() {
        let mut budget = Budget::new(512);
        let original = Arena::new(&mut budget, "overlapping retained result", 512).unwrap();
        let image = crate::mem::guard::forbid_alloc(|| {
            original
                .with_remaining_retained_bytes(&original, |child| {
                    child.alloc_slice_with(16, |_| 0u8)?;
                    child
                        .alloc_slice_with(300, |at| (at * 37 % 251) as u8)
                        .map(|bytes| &*bytes)
                })
                .unwrap()
                .unwrap()
        });
        let mut moved = original;
        crate::mem::guard::forbid_alloc(|| {
            moved.recycle_front();
            let used = moved.used();
            assert!(
                moved
                    .with_remaining_retained_bytes(&moved, |child| {
                        child.alloc_slice_with(180, |_| 0xffu8).map(|bytes| &*bytes)
                    })
                    .is_err()
            );
            assert_eq!(moved.used(), used);
            assert!(matches!(
                moved
                    .with_remaining_retained_bytes(&moved, |_| Err::<&[u8], _>(7))
                    .unwrap(),
                Err(7)
            ));
            moved.alloc_slice_with(128, |_| 0xccu8).unwrap();
            let bytes = moved.retained_slice(image).unwrap();
            assert_eq!(bytes.len(), 300);
            for (at, byte) in bytes.iter().enumerate() {
                assert_eq!(*byte, (at * 37 % 251) as u8);
            }
            moved.reset();
            assert!(matches!(
                moved.retained_slice(image),
                Err(ArenaSliceError::Stale)
            ));
        });
    }

    #[test]
    fn retained_region_transfers_publish_into_borrowed_parents_without_pinning_body_scratch() {
        let mut budget = Budget::new(4096);
        let owner = Arena::new(&mut budget, "nested retained result", 4096).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let image = owner
                .with_remaining_retained_bytes(&owner, |parent| {
                    let nested = parent.with_remaining_retained_bytes(parent, |child| {
                        child
                            .alloc_slice_with(300, |at| (at * 37 % 251) as u8)
                            .map(|bytes| &*bytes)
                    })??;
                    let used = parent.used();
                    assert!(used <= Arena::retained_slice_capacity::<u8>(300).unwrap());
                    let mark = parent.mark();
                    parent.alloc_slice_with(1024, |_| 0xccu8)?;
                    // The discarded suffix contains no referenced values.
                    unsafe { parent.rewind_to(mark) };
                    parent.with_remaining_region(|child| {
                        child.alloc_slice_with(1024, |_| 0xddu8).unwrap();
                    })?;
                    assert_eq!(parent.used(), used);
                    parent.retained_slice(nested).map_err(|_| parent.full(300))
                })
                .unwrap()
                .unwrap();
            owner.alloc_slice_with(2048, |_| 0xeeu8).unwrap();
            let bytes = owner.retained_slice(image).unwrap();
            assert_eq!(bytes.len(), 300);
            for (at, byte) in bytes.iter().enumerate() {
                assert_eq!(*byte, (at * 37 % 251) as u8);
            }
        });
    }

    #[test]
    fn frozen_retained_slices_share_views_and_permanently_reject_mutable_aliases() {
        let mut budget = Budget::new(1024);
        let original = Arena::new(&mut budget, "frozen bytes", 512).unwrap();
        let other = Arena::new(&mut budget, "other frozen bytes", 512).unwrap();
        let mutable = original.retain_slice_with(4, |index| index as u64).unwrap();
        let mut borrowed = original.retained_slice_mut(mutable).unwrap();
        borrowed[1] = 42;
        assert!(matches!(
            original.freeze_retained_slice(mutable),
            Err(ArenaSliceError::Borrowed)
        ));
        drop(borrowed);
        let frozen = original.freeze_retained_slice(mutable).unwrap();
        let mut moved = original;
        crate::mem::guard::forbid_alloc(|| {
            moved.recycle_front();
            let first = moved.retained_slice(frozen).unwrap();
            let second = moved.retained_slice(frozen).unwrap();
            assert_eq!(first, &[0, 42, 2, 3]);
            assert_eq!(first.as_ptr(), second.as_ptr());
            assert!(matches!(
                moved.retained_slice_mut(mutable),
                Err(ArenaSliceError::Frozen)
            ));
            assert!(matches!(
                moved.freeze_retained_slice(mutable),
                Err(ArenaSliceError::Frozen)
            ));
            assert!(matches!(
                other.retained_slice(frozen),
                Err(ArenaSliceError::Stale)
            ));
            moved.reset();
            assert!(matches!(
                moved.retained_slice(frozen),
                Err(ArenaSliceError::Stale)
            ));
        });
    }

    #[test]
    fn arena_retained_slices_reject_aliases_and_survive_moves_and_front_rewinds() {
        let mut budget = Budget::new(4096);
        let original = Arena::new(&mut budget, "retained slices", 2048).unwrap();
        let other = Arena::new(&mut budget, "other retained slices", 2048).unwrap();
        let first = original.retain_slice_with(3, |at| at as u64).unwrap();
        let second = original.retain_slice_with(2, |_| 41u64).unwrap();
        let mut moved = original;
        crate::mem::guard::forbid_alloc(|| {
            let mark = moved.mark();
            moved.alloc_slice_with(16, |_| 0xffu8).unwrap();
            unsafe { moved.rewind_to(mark) };
            let mut one = moved.retained_slice_mut(first).unwrap();
            assert!(matches!(
                moved.retained_slice_mut(first),
                Err(ArenaSliceError::Borrowed)
            ));
            let mut two = moved.retained_slice_mut(second).unwrap();
            one[1] = two[0] + 1;
            two[1] = one[1] + 1;
            assert_eq!(&*one, &[0, 42, 2]);
            assert_eq!(&*two, &[41, 43]);
            drop(one);
            drop(two);
            assert_eq!(&*moved.retained_slice_mut(first).unwrap(), &[0, 42, 2]);
            assert!(matches!(
                other.retained_slice_mut(first),
                Err(ArenaSliceError::Stale)
            ));
            moved.reset();
            assert!(matches!(
                moved.retained_slice_mut(first),
                Err(ArenaSliceError::Stale)
            ));
            let replacement = moved.retain_slice_with(3, |_| 99u64).unwrap();
            assert!(matches!(
                moved.retained_slice_mut(first),
                Err(ArenaSliceError::Stale)
            ));
            assert_eq!(&*moved.retained_slice_mut(replacement).unwrap(), &[99; 3]);
        });
    }

    #[test]
    fn arena_retained_slices_release_borrows_on_unwind_and_check_capacity() {
        let mut budget = Budget::new(256);
        let arena = Arena::new(&mut budget, "slice failure cleanup", 256).unwrap();
        let handle = arena.retain_slice_with(1, |_| 7u128).unwrap();
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut values = arena.retained_slice_mut(handle).unwrap();
            values[0] = 42;
            panic!("leave the retained slice");
        }));
        assert!(unwind.is_err());
        assert_eq!(&*arena.retained_slice_mut(handle).unwrap(), &[42]);
        let used = arena.used();
        assert!(arena.retain_slice_with(usize::MAX, |_| 0u64).is_err());
        assert_eq!(arena.used(), used);
        assert!(arena.retain_slice_with(256, |_| 0u8).is_err());
        assert_eq!(arena.used(), used);
        let empty = arena.retain_slice_with(0, |_| 0u64).unwrap();
        assert!(arena.retained_slice_mut(empty).unwrap().is_empty());
    }

    #[test]
    fn arena_retained_slices_recycle_temporary_rows_without_losing_positions() {
        let mut budget = Budget::new(512);
        let mut arena =
            Arena::new(&mut budget, "retained positions and temporary rows", 512).unwrap();
        let positions = arena.retain_slice_with(4, |_| 0u64).unwrap();
        let persistent_bytes = arena.used();
        crate::mem::guard::forbid_alloc(|| {
            for turn in 0..32u8 {
                arena.recycle_front();
                let row = arena.alloc_slice_with(256, |_| turn).unwrap();
                arena.retained_slice_mut(positions).unwrap()[turn as usize % 4] += row.len() as u64;
            }
            arena.recycle_front();
            assert_eq!(arena.used(), persistent_bytes);
            assert_eq!(&*arena.retained_slice_mut(positions).unwrap(), &[2048; 4]);
        });
    }
}
