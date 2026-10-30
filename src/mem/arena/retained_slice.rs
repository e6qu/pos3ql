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

struct SliceHeader {
    len: usize,
    borrowed: Cell<bool>,
}

fn slice_layout<T>(len: usize) -> Result<(Layout, usize), std::alloc::LayoutError> {
    Layout::new::<SliceHeader>().extend(Layout::array::<T>(len)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArenaSliceError {
    Stale,
    Borrowed,
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
    /// Upper bound including alignment when the preceding tail is unaligned.
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
        if handle.identity != self.retention_identity {
            return Err(ArenaSliceError::Stale);
        }
        // Matching non-reused identity proves this typed header and its whole
        // initialized payload are still in this arena's persistent tail.
        let base = unsafe { self.base.add(handle.offset) };
        let header = unsafe { &*base.cast::<SliceHeader>() };
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
