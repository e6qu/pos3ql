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
    tail_offset: Cell<usize>,
    high_water: Cell<usize>,
}

/// An allocation frontier retained while short-lived work above it is
/// recycled. The marker is arena-specific and intentionally opaque.
#[derive(Clone, Copy)]
pub(crate) struct ArenaMark {
    arena: *const Arena,
    offset: usize,
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
            tail_offset: Cell::new(capacity),
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

    pub(crate) fn alloc_atomic_usize(
        &self,
        value: usize,
    ) -> Result<&core::sync::atomic::AtomicUsize, ArenaFull> {
        let ptr = self
            .alloc_raw(Layout::new::<core::sync::atomic::AtomicUsize>())?
            .cast::<core::sync::atomic::AtomicUsize>();
        unsafe {
            ptr.write(core::sync::atomic::AtomicUsize::new(value));
            Ok(&*ptr)
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
        let mut writer = SliceWriter {
            buffer: bytes,
            at: 0,
        };
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

    /// Allocates persistent statement state from the opposite end of the
    /// arena. Front rewinds used for per-row scratch cannot reclaim it.
    #[expect(
        clippy::mut_from_ref,
        reason = "each call returns a disjoint tail region; reset() takes &mut self"
    )]
    pub(crate) fn alloc_persistent_slice_with<T: Copy>(
        &self,
        len: usize,
        mut fill: impl FnMut(usize) -> T,
    ) -> Result<&mut [T], ArenaFull> {
        let layout = Layout::array::<T>(len).map_err(|_| self.full(usize::MAX))?;
        let ptr = self.alloc_raw_tail(layout)?.cast::<T>();
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
        self.tail_offset.set(self.capacity);
    }

    /// Marks the current frontier for allocation-free per-row scratch reuse.
    pub(crate) fn mark(&self) -> ArenaMark {
        ArenaMark {
            arena: self,
            offset: self.offset.get(),
        }
    }

    /// Recycles every front allocation made after `mark`. Persistent tail
    /// allocations remain live until the whole arena is reset.
    ///
    /// # Safety
    ///
    /// No reference into the discarded suffix may be used after this call.
    /// This is reserved for executor choke points that encode or emit every
    /// per-row value before rewinding; long-lived AST/scope state is allocated
    /// below the mark.
    pub(crate) unsafe fn rewind_to(&self, mark: ArenaMark) {
        assert!(
            core::ptr::eq(mark.arena, self),
            "arena mark belongs to another arena"
        );
        assert!(
            mark.offset <= self.offset.get(),
            "arena mark is ahead of the frontier"
        );
        self.offset.set(mark.offset);
    }

    pub fn used(&self) -> usize {
        self.offset.get() + self.capacity - self.tail_offset.get()
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
        let end = start
            .checked_add(layout.size())
            .ok_or_else(|| self.full(layout.size()))?;
        if end > self.tail_offset.get() {
            return Err(self.full(layout.size()));
        }
        self.offset.set(end);
        let used = end + self.capacity - self.tail_offset.get();
        if used > self.high_water.get() {
            self.high_water.set(used);
        }
        Ok(unsafe { self.base.add(start) })
    }

    fn alloc_raw_tail(&self, layout: Layout) -> Result<*mut u8, ArenaFull> {
        assert!(
            layout.align() <= ARENA_ALIGN,
            "arena '{}': alignment {} exceeds arena alignment {}",
            self.what,
            layout.align(),
            ARENA_ALIGN
        );
        let unaligned = self
            .tail_offset
            .get()
            .checked_sub(layout.size())
            .ok_or_else(|| self.full(layout.size()))?;
        let start = unaligned & !(layout.align() - 1);
        if start < self.offset.get() {
            return Err(self.full(layout.size()));
        }
        self.tail_offset.set(start);
        let used = self.offset.get() + self.capacity - start;
        if used > self.high_water.get() {
            self.high_water.set(used);
        }
        Ok(unsafe { self.base.add(start) })
    }

    fn full(&self, requested: usize) -> ArenaFull {
        ArenaFull {
            what: self.what,
            requested,
            remaining: self.tail_offset.get() - self.offset.get(),
            capacity: self.capacity,
        }
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        let layout =
            Layout::from_size_align(self.capacity, ARENA_ALIGN).expect("validated at construction");
        unsafe { std::alloc::dealloc(self.base, layout) };
    }
}

/// A geometrically growing list whose backing storage comes from one arena.
/// Growth leaves the old prefix in the bump arena, so all abandoned storage
/// is bounded by the final capacity and reclaimed with the arena.
pub(crate) struct ArenaList<'a, T: Copy> {
    arena: &'a Arena,
    entries: *mut T,
    len: usize,
    capacity: usize,
    persistent: bool,
}

impl<'a, T: Copy + 'a> ArenaList<'a, T> {
    pub(crate) const fn new(arena: &'a Arena) -> Self {
        Self {
            arena,
            entries: core::ptr::null_mut(),
            len: 0,
            capacity: 0,
            persistent: false,
        }
    }

    /// Creates a list whose backing buffers survive front rewinds. Use this
    /// for state retained across executor attempts or per-row scratch scopes.
    pub(crate) const fn new_persistent(arena: &'a Arena) -> Self {
        Self {
            arena,
            entries: core::ptr::null_mut(),
            len: 0,
            capacity: 0,
            persistent: true,
        }
    }

    pub(crate) fn push(&mut self, value: T) -> Result<(), ArenaFull> {
        if self.len == self.capacity {
            let capacity = self.capacity.saturating_mul(2).max(4);
            let entries = if self.persistent {
                self.arena
                    .alloc_persistent_slice_with(capacity, |_| value)?
            } else {
                self.arena.alloc_slice_with(capacity, |_| value)?
            };
            if self.len != 0 {
                unsafe {
                    core::ptr::copy_nonoverlapping(self.entries, entries.as_mut_ptr(), self.len);
                }
            }
            self.entries = entries.as_mut_ptr();
            self.capacity = capacity;
        }
        unsafe {
            self.entries.add(self.len).write(value);
        }
        self.len += 1;
        Ok(())
    }

    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    /// Drops entries past `len` without reclaiming their arena storage (the
    /// bump arena reclaims everything at reset). `len` must not exceed the
    /// current length.
    pub(crate) fn truncate(&mut self, len: usize) {
        assert!(len <= self.len);
        self.len = len;
    }

    pub(crate) const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(crate) fn as_slice(&self) -> &'a [T] {
        if self.is_empty() {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(self.entries, self.len) }
        }
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [T] {
        if self.is_empty() {
            &mut []
        } else {
            unsafe { core::slice::from_raw_parts_mut(self.entries, self.len) }
        }
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
            let mut a: Vec<(u32, u32)> = (0..n as u32)
                .map(|i| (i.wrapping_mul(2_654_435_761) % 17, i))
                .collect();
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

    #[test]
    fn arena_list_grows_geometrically_without_heap_allocation() {
        let mut budget = Budget::new(8192);
        let arena = Arena::new(&mut budget, "list", 4096).unwrap();
        let mut list = ArenaList::new(&arena);
        crate::mem::guard::forbid_alloc(|| {
            for value in 0..70u32 {
                list.push(value).unwrap();
            }
        });
        assert_eq!(list.len(), 70);
        assert_eq!(list.as_slice(), (0..70u32).collect::<Vec<_>>());
    }

    #[test]
    fn arena_list_reports_arena_exhaustion() {
        let mut budget = Budget::new(1024);
        let arena = Arena::new(&mut budget, "small_list", 64).unwrap();
        let mut list = ArenaList::new(&arena);
        let error = loop {
            if let Err(error) = list.push(1u64) {
                break error;
            }
        };
        assert_eq!(error.what, "small_list");
        assert_eq!(list.as_slice(), &[1, 1, 1, 1]);
    }

    #[test]
    fn persistent_arena_list_survives_front_rewinds() {
        let mut budget = Budget::new(4096);
        let arena = Arena::new(&mut budget, "two ended", 2048).unwrap();
        let mark = arena.mark();
        let mut list = ArenaList::new_persistent(&arena);
        for value in 0..70u32 {
            let _scratch = arena.alloc_slice_with(16, |_| 0xa5u8).unwrap();
            list.push(value).unwrap();
            unsafe { arena.rewind_to(mark) };
        }
        let _overwrite = arena.alloc_slice_with(512, |_| 0x5au8).unwrap();
        assert_eq!(list.as_slice(), (0..70u32).collect::<Vec<_>>());
    }
}
