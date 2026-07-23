//! A bump arena: allocation is a pointer increment, deallocation only
//! happens wholesale via `reset`. Backs per-request state (SQL ASTs,
//! row scratch) that lives exactly as long as one request.
//!
//! Values are restricted to `Copy` because the arena never runs
//! destructors: `reset` just rewinds the offset.

use core::cell::Cell;
use core::fmt;
use std::alloc::Layout;

use super::budget::{Budget, BudgetError};

pub struct Arena {
    what: &'static str,
    base: *mut u8,
    capacity: usize,
    offset: Cell<usize>,
    high_water: Cell<usize>,
}

// The arena owns its buffer; the raw pointer is not shared outside the
// lifetimes handed out by `alloc*`, so moving the arena to another thread is
// sound. `Cell` keeps it !Sync, which is correct.
unsafe impl Send for Arena {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArenaFull {
    pub what: &'static str,
    pub requested: usize,
    pub remaining: usize,
    pub capacity: usize,
}

impl fmt::Display for ArenaFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "arena '{}' full: requested {} bytes, {} of {} remaining",
            self.what, self.requested, self.remaining, self.capacity
        )
    }
}

impl std::error::Error for ArenaFull {}

const ARENA_ALIGN: usize = 16;

impl Arena {
    pub fn new(
        budget: &mut Budget,
        what: &'static str,
        capacity: usize,
    ) -> Result<Self, BudgetError> {
        assert!(capacity > 0, "arena '{what}' capacity must be non-zero");
        budget.draw(capacity, what)?;
        let layout = Layout::from_size_align(capacity, ARENA_ALIGN)
            .unwrap_or_else(|_| panic!("arena '{what}' capacity {capacity} is unrepresentable"));
        let base = unsafe { std::alloc::alloc(layout) };
        assert!(!base.is_null(), "arena '{what}' allocation failed");
        Ok(Self {
            what,
            base,
            capacity,
            offset: Cell::new(0),
            high_water: Cell::new(0),
        })
    }

    #[expect(
        clippy::mut_from_ref,
        reason = "each call returns a disjoint region; reset() takes &mut self, so no returned borrow can outlive rewinding"
    )]
    pub fn alloc<T: Copy>(&self, value: T) -> Result<&mut T, ArenaFull> {
        let ptr = self.alloc_raw(Layout::new::<T>())?.cast::<T>();
        unsafe {
            ptr.write(value);
            Ok(&mut *ptr)
        }
    }

    #[expect(
        clippy::mut_from_ref,
        reason = "each call returns a disjoint region; reset() takes &mut self, so no returned borrow can outlive rewinding"
    )]
    pub fn alloc_slice_copy<T: Copy>(&self, src: &[T]) -> Result<&mut [T], ArenaFull> {
        let layout = Layout::array::<T>(src.len()).map_err(|_| self.full(usize::MAX))?;
        let ptr = self.alloc_raw(layout)?.cast::<T>();
        unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr(), ptr, src.len());
            Ok(core::slice::from_raw_parts_mut(ptr, src.len()))
        }
    }

    pub fn alloc_str(&self, src: &str) -> Result<&str, ArenaFull> {
        let bytes = self.alloc_slice_copy(src.as_bytes())?;
        Ok(unsafe { core::str::from_utf8_unchecked(bytes) })
    }

    /// Renders a `Display` value straight into the arena at its exact length —
    /// no fixed-size scratch buffer, so arbitrarily long values (JSON, arrays,
    /// ranges) never truncate. Measures once, then writes once.
    pub fn alloc_str_display(&self, value: impl core::fmt::Display) -> Result<&str, ArenaFull> {
        use core::fmt::Write;
        struct Counter(usize);
        impl Write for Counter {
            fn write_str(&mut self, s: &str) -> core::fmt::Result {
                self.0 += s.len();
                Ok(())
            }
        }
        let mut counter = Counter(0);
        // Display's fmt is total for our Datum types, so this never errors.
        let _ = write!(counter, "{value}");
        let bytes = self.alloc_slice_with(counter.0, |_| 0u8)?;
        struct SliceWriter<'a> {
            buffer: &'a mut [u8],
            at: usize,
        }
        impl Write for SliceWriter<'_> {
            fn write_str(&mut self, s: &str) -> core::fmt::Result {
                let end = self.at + s.len();
                self.buffer[self.at..end].copy_from_slice(s.as_bytes());
                self.at = end;
                Ok(())
            }
        }
        let mut writer = SliceWriter { buffer: bytes, at: 0 };
        let _ = write!(writer, "{value}");
        Ok(unsafe { core::str::from_utf8_unchecked(writer.buffer) })
    }

    #[expect(
        clippy::mut_from_ref,
        reason = "each call returns a disjoint region; reset() takes &mut self, so no returned borrow can outlive rewinding"
    )]
    pub fn alloc_slice_with<T: Copy>(
        &self,
        len: usize,
        mut fill: impl FnMut(usize) -> T,
    ) -> Result<&mut [T], ArenaFull> {
        let layout = Layout::array::<T>(len).map_err(|_| self.full(usize::MAX))?;
        let ptr = self.alloc_raw(layout)?.cast::<T>();
        unsafe {
            for i in 0..len {
                ptr.add(i).write(fill(i));
            }
            Ok(core::slice::from_raw_parts_mut(ptr, len))
        }
    }

    /// Rewinds the arena. Requires `&mut self`, so the borrow checker
    /// guarantees no allocation handed out earlier is still alive.
    pub fn reset(&mut self) {
        self.offset.set(0);
    }

    pub fn used(&self) -> usize {
        self.offset.get()
    }

    /// Highest fill ever reached — observability for sizing the arena.
    pub fn high_water(&self) -> usize {
        self.high_water.get()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    fn alloc_raw(&self, layout: Layout) -> Result<*mut u8, ArenaFull> {
        assert!(
            layout.align() <= ARENA_ALIGN,
            "arena '{}': alignment {} exceeds arena alignment {}",
            self.what,
            layout.align(),
            ARENA_ALIGN
        );
        let start = self.offset.get().next_multiple_of(layout.align());
        let end = start.checked_add(layout.size()).ok_or_else(|| self.full(layout.size()))?;
        if end > self.capacity {
            return Err(self.full(layout.size()));
        }
        self.offset.set(end);
        if end > self.high_water.get() {
            self.high_water.set(end);
        }
        Ok(unsafe { self.base.add(start) })
    }

    fn full(&self, requested: usize) -> ArenaFull {
        ArenaFull {
            what: self.what,
            requested,
            remaining: self.capacity - self.offset.get(),
            capacity: self.capacity,
        }
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.capacity, ARENA_ALIGN)
            .expect("validated at construction");
        unsafe { std::alloc::dealloc(self.base, layout) };
    }
}

/// A stable sort that never touches the allocator: the standard library's
/// stable `sort_by` draws merge scratch from the heap for large slices, which
/// the post-startup allocation guard forbids. This stages a permutation in
/// `arena` instead — an unstable sort over indices with the original position
/// as the tiebreak reproduces stability exactly — then applies it in place by
/// following cycles.
pub fn stable_sort_via<T>(
    arena: &Arena,
    items: &mut [T],
    mut cmp: impl FnMut(&T, &T) -> core::cmp::Ordering,
) -> Result<(), ArenaFull> {
    let n = items.len();
    if n < 2 {
        return Ok(());
    }
    let perm = arena.alloc_slice_with(n, |i| i as u32)?;
    perm.sort_unstable_by(|&a, &b| {
        cmp(&items[a as usize], &items[b as usize]).then_with(|| a.cmp(&b))
    });
    // Apply the permutation by cycles: each element moves at most once.
    let visited = arena.alloc_slice_with(n, |_| false)?;
    for start in 0..n {
        if visited[start] || perm[start] as usize == start {
            visited[start] = true;
            continue;
        }
        let mut at = start;
        loop {
            visited[at] = true;
            let from = perm[at] as usize;
            if from == start {
                break;
            }
            items.swap(at, from);
            at = from;
            if visited[at] {
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn stable_sort_via_matches_the_standard_stable_sort() {
        // Duplicate keys in scrambled order: the arena-staged sort must keep
        // their original relative order exactly as the (allocating) standard
        // stable sort does, across sizes on both sides of driftsort's
        // stack-scratch threshold.
        let mut budget = crate::mem::budget::Budget::new(64 << 20);
        let mut arena = super::Arena::new(&mut budget, "sort", 16 << 20).unwrap();
        for n in [0usize, 1, 2, 7, 33, 1000, 30_000] {
            let mut a: Vec<(u32, u32)> =
                (0..n as u32).map(|i| (i.wrapping_mul(2_654_435_761) % 17, i)).collect();
            let mut b = a.clone();
            a.sort_by_key(|x| x.0);
            super::stable_sort_via(&arena, &mut b, |x, y| x.0.cmp(&y.0)).unwrap();
            assert_eq!(a, b, "n = {n}");
            arena.reset();
        }
    }

    use super::*;

    #[test]
    fn values_and_slices_coexist() {
        let mut budget = Budget::new(1024);
        let arena = Arena::new(&mut budget, "test", 256).unwrap();
        let a = arena.alloc(42u64).unwrap();
        let s = arena.alloc_str("hello").unwrap();
        let b = arena.alloc([1u32, 2, 3]).unwrap();
        assert_eq!(*a, 42);
        assert_eq!(s, "hello");
        assert_eq!(*b, [1, 2, 3]);
    }

    #[test]
    fn exhaustion_is_a_named_error() {
        let mut budget = Budget::new(1024);
        let arena = Arena::new(&mut budget, "sql_ast", 32).unwrap();
        arena.alloc([0u8; 30]).unwrap();
        let err = arena.alloc([0u8; 8]).unwrap_err();
        assert_eq!(err.what, "sql_ast");
        assert_eq!(err.requested, 8);
        assert_eq!(err.capacity, 32);
    }

    #[test]
    fn reset_reclaims_everything() {
        let mut budget = Budget::new(1024);
        let mut arena = Arena::new(&mut budget, "test", 64).unwrap();
        arena.alloc([0u8; 60]).unwrap();
        assert!(arena.alloc(0u64).is_err());
        arena.reset();
        assert_eq!(arena.used(), 0);
        assert_eq!(arena.high_water(), 60);
        arena.alloc([0u8; 60]).unwrap();
    }

    #[test]
    fn alignment_is_respected() {
        let mut budget = Budget::new(1024);
        let arena = Arena::new(&mut budget, "test", 256).unwrap();
        arena.alloc(1u8).unwrap();
        let x = arena.alloc(2u64).unwrap();
        assert_eq!((x as *mut u64 as usize) % align_of::<u64>(), 0);
        arena.alloc(3u8).unwrap();
        let y = arena.alloc(4u128).unwrap();
        assert_eq!((y as *mut u128 as usize) % align_of::<u128>(), 0);
    }

    #[test]
    fn arena_allocs_do_not_hit_the_heap() {
        let mut budget = Budget::new(8192);
        let arena = Arena::new(&mut budget, "test", 4096).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            for i in 0..100u64 {
                arena.alloc(i).unwrap();
            }
        });
    }
}
