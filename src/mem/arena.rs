//! A bump arena: allocation is a pointer increment, deallocation only
//! happens wholesale via `reset`. Backs per-request state (SQL ASTs,
//! row scratch) that lives exactly as long as one request.
//!
//! Ordinary allocations require Copy because reset does not run destructors.
//! Scoped owned slices release their values before the arena can reset.

use core::cell::Cell;
use core::fmt;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicU64, Ordering};
use std::alloc::Layout;

use super::budget::{Budget, BudgetError};

pub struct Arena {
    what: &'static str,
    base: *mut u8,
    capacity: usize,
    offset: Cell<usize>,
    tail_offset: Cell<usize>,
    high_water: Cell<usize>,
    retention_identity: u64,
    interned: Cell<Option<core::ptr::NonNull<InternedAllocation>>>,
}

#[derive(Clone, Copy)]
struct InternedAllocation {
    value_type: core::any::TypeId,
    value: *const u8,
    end: usize,
    previous: Option<core::ptr::NonNull<InternedAllocation>>,
}

#[derive(Clone, Copy)]
pub(crate) struct InternedValue<T> {
    value: T,
    identity: InternedAllocation,
}

static NEXT_RETENTION_IDENTITY: AtomicU64 = AtomicU64::new(1);

fn retention_identity() -> u64 {
    let mut identity = NEXT_RETENTION_IDENTITY.load(Ordering::Relaxed);
    loop {
        let Some(next) = identity.checked_add(1) else {
            return 0;
        };
        match NEXT_RETENTION_IDENTITY.compare_exchange_weak(
            identity,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return identity,
            Err(current) => identity = current,
        }
    }
}

/// A copied value in the persistent tail. Handles reject a reset, replacement
/// or different arena before accessing bytes; they do not retain references.
pub(crate) struct ArenaHandle<T> {
    identity: u64,
    offset: usize,
    // Keep T's thread-safety requirements and forbid shortening a stored
    // reference's lifetime through one clone while retaining another clone.
    value: PhantomData<T>,
    invariant: PhantomData<fn(T) -> T>,
}

impl<T> Copy for ArenaHandle<T> {}

impl<T> Clone for ArenaHandle<T> {
    fn clone(&self) -> Self {
        *self
    }
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
            retention_identity: retention_identity(),
            interned: Cell::new(None),
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

    /// Shares immutable values already retained by this arena. The linked
    /// identities consume the same fixed arena reservation as their values;
    /// rewind discards their entries together with the allocations they describe.
    pub(crate) fn alloc_interned<T: Copy + Send + Sync + 'static>(
        &self,
        value: T,
        mut equivalent: impl FnMut(&T, &T) -> bool,
    ) -> Result<&T, ArenaFull> {
        let mut cursor = self.interned.get();
        while let Some(previous) = cursor {
            // Only this method installs entries, after initializing their T.
            // Rewind removes every entry whose allocation it discards.
            let previous = unsafe { previous.as_ref() };
            if previous.value_type == core::any::TypeId::of::<T>() {
                let previous_value = unsafe { &*previous.value.cast::<T>() };
                if equivalent(previous_value, &value) {
                    return Ok(previous_value);
                }
            }
            cursor = previous.previous;
        }
        let allocated = self.alloc(InternedValue { value, identity: InternedAllocation {
            value_type: core::any::TypeId::of::<T>(),
            value: core::ptr::null(),
            end: 0,
            previous: self.interned.get(),
        }})?;
        allocated.identity.value = (&allocated.value as *const T).cast();
        allocated.identity.end = self.offset.get();
        self.interned.set(Some(core::ptr::NonNull::from(&mut allocated.identity)));
        Ok(&allocated.value)
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

    /// Retains values with destructors until this scoped owner is dropped.
    /// Tail storage survives front rewinds; reset requires the owner to end.
    pub(crate) fn alloc_owned_slice_with<T>(
        &self,
        len: usize,
        mut fill: impl FnMut(usize) -> T,
    ) -> Result<OwnedArenaSlice<'_, T>, ArenaFull> {
        let layout = Layout::array::<T>(len).map_err(|_| self.full(usize::MAX))?;
        let entries = self.alloc_raw_tail(layout)?.cast::<T>();
        let mut owned = OwnedArenaSlice {
            entries,
            len: 0,
            arena: core::marker::PhantomData,
        };
        for index in 0..len {
            // SAFETY: this disjoint tail allocation has room and alignment for
            // every entry. Publish initialization only after fill succeeds.
            unsafe { entries.add(index).write(fill(index)) };
            owned.len += 1;
        }
        Ok(owned)
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
        self.interned.set(None);
        self.offset.set(0);
        self.tail_offset.set(self.capacity);
        self.retention_identity = retention_identity();
    }

    pub(crate) fn retain<T: Copy>(&self, value: T) -> Result<ArenaHandle<T>, ArenaFull> {
        if self.retention_identity == 0 {
            return Err(ArenaFull {
                what: "arena retained identity space exhausted",
                requested: 1,
                remaining: 0,
                capacity: 0,
            });
        }
        let retained = self.alloc_persistent_slice_with(1, |_| value)?;
        Ok(ArenaHandle {
            identity: self.retention_identity,
            offset: retained.as_ptr() as usize - self.base as usize,
            value: PhantomData,
            invariant: PhantomData,
        })
    }

    pub(crate) fn retained<T: Copy>(&self, handle: ArenaHandle<T>) -> Option<T> {
        if handle.identity != self.retention_identity {
            return None;
        }
        // SAFETY: a matching non-reused identity proves retain initialized this
        // typed slot in this arena's current tail. Only copies escape.
        Some(unsafe { self.base.add(handle.offset).cast::<T>().read() })
    }

    pub(crate) fn replace_retained<T: Copy>(&self, handle: ArenaHandle<T>, value: T) -> bool {
        if handle.identity != self.retention_identity {
            return false;
        }
        // SAFETY: retain exposes only opaque handles, so this initialized slot
        // cannot have outstanding references. Arena is not Sync.
        unsafe { self.base.add(handle.offset).cast::<T>().write(value) };
        true
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
        while let Some(cached) = self.interned.get() {
            // Entries remain initialized until the frontier moves below them.
            let cached = unsafe { cached.as_ref() };
            if cached.end <= mark.offset {
                break;
            }
            self.interned.set(cached.previous);
        }
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

/// Owns only initialized values; backing storage remains in the fixed arena.
pub(crate) struct OwnedArenaSlice<'arena, T> {
    entries: *mut T,
    len: usize,
    arena: core::marker::PhantomData<&'arena Arena>,
}

impl<T> core::ops::Deref for OwnedArenaSlice<'_, T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        // SAFETY: initialization advances len, the arena outlives this owner,
        // and this borrow cannot outlive the values' scoped owner.
        unsafe { core::slice::from_raw_parts(self.entries, self.len) }
    }
}

impl<T> core::ops::DerefMut for OwnedArenaSlice<'_, T> {
    fn deref_mut(&mut self) -> &mut [T] {
        // SAFETY: the owner is unique and its exclusive borrow excludes access
        // through any earlier slice returned by Deref or DerefMut.
        unsafe { core::slice::from_raw_parts_mut(self.entries, self.len) }
    }
}

impl<T> Drop for OwnedArenaSlice<'_, T> {
    fn drop(&mut self) {
        // SAFETY: only initialized entries are included, including when fill
        // panics. No safe borrow can survive destruction of this owner.
        unsafe {
            core::ptr::drop_in_place(core::ptr::slice_from_raw_parts_mut(self.entries, self.len));
        }
    }
}

/// A geometrically growing list whose backing storage comes from one arena.
/// Growth and edits preserve published slices. All backing buffers stay
/// within the fixed arena and are reclaimed when it resets.
pub(crate) struct ArenaList<'a, T: Copy> {
    arena: &'a Arena,
    entries: *mut T,
    len: usize,
    capacity: usize,
    persistent: bool,
    exposed: Cell<bool>,
}

impl<'a, T: Copy + 'a> ArenaList<'a, T> {
    pub(crate) const fn new(arena: &'a Arena) -> Self {
        Self {
            arena,
            entries: core::ptr::null_mut(),
            len: 0,
            capacity: 0,
            persistent: false,
            exposed: Cell::new(false),
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
            exposed: Cell::new(false),
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
            self.exposed.set(false);
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
        if len < self.len && self.exposed.get() {
            // Appending must not overwrite an already published suffix.
            self.capacity = len;
        }
        self.len = len;
    }

    pub(crate) const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(crate) fn as_slice(&self) -> &'a [T] {
        if self.is_empty() {
            &[]
        } else {
            self.exposed.set(true);
            // Published slices remain immutable for the arena lifetime.
            unsafe { core::slice::from_raw_parts(self.entries, self.len) }
        }
    }

    /// A short borrow for inspection before further edits to this list.
    pub(crate) fn borrowed_slice(&self) -> &[T] {
        if self.is_empty() {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(self.entries, self.len) }
        }
    }

    fn make_unique(&mut self) -> Result<(), ArenaFull> {
        if !self.exposed.get() {
            return Ok(());
        }
        if self.is_empty() {
            self.entries = core::ptr::null_mut();
            self.capacity = 0;
        } else {
            // Existing arena-lifetime slices must retain their old values.
            let first = unsafe { *self.entries };
            let entries = if self.persistent {
                self.arena
                    .alloc_persistent_slice_with(self.capacity, |_| first)?
            } else {
                self.arena.alloc_slice_with(self.capacity, |_| first)?
            };
            unsafe {
                core::ptr::copy_nonoverlapping(self.entries, entries.as_mut_ptr(), self.len);
            }
            self.entries = entries.as_mut_ptr();
        }
        self.exposed.set(false);
        Ok(())
    }

    pub(crate) fn as_mut_slice(&mut self) -> Result<&mut [T], ArenaFull> {
        self.make_unique()?;
        if self.is_empty() {
            Ok(&mut [])
        } else {
            Ok(unsafe { core::slice::from_raw_parts_mut(self.entries, self.len) })
        }
    }

    pub(crate) fn into_mut_slice(mut self) -> Result<&'a mut [T], ArenaFull> {
        self.make_unique()?;
        if self.is_empty() {
            Ok(&mut [])
        } else {
            // The consumed list cannot edit this storage again, and any
            // previously published immutable slices were detached above.
            Ok(unsafe { core::slice::from_raw_parts_mut(self.entries, self.len) })
        }
    }
}

/// A growable UTF-8 string built in the arena, so text results are bounded by
/// statement memory rather than a fixed stack buffer. Exhaustion surfaces as
/// `Err` from the `fmt::Write` methods.
pub(crate) struct ArenaString<'a> {
    bytes: ArenaList<'a, u8>,
}

impl<'a> ArenaString<'a> {
    pub(crate) const fn new(arena: &'a Arena) -> Self {
        Self {
            bytes: ArenaList::new(arena),
        }
    }

    /// The accumulated text. Valid UTF-8 because only `&str` data is pushed.
    pub(crate) fn as_str(&self) -> &'a str {
        // UTF-8 validity: every push comes from a &str.
        unsafe { core::str::from_utf8_unchecked(self.bytes.as_slice()) }
    }
}

impl core::fmt::Write for ArenaString<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &byte in s.as_bytes() {
            self.bytes.push(byte).map_err(|_| core::fmt::Error)?;
        }
        Ok(())
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
    fn arena_retained_handles_survive_moves_and_rewinds_but_reject_reset_and_replacement() {
        let mut budget = Budget::new(4096);
        let original = Arena::new(&mut budget, "retained values", 2048).unwrap();
        let other = Arena::new(&mut budget, "other retained values", 2048).unwrap();
        let handle = original.retain(41u64).unwrap();
        let mut moved = original;
        crate::mem::guard::forbid_alloc(|| {
            let mark = moved.mark();
            moved.alloc_slice_with(32, |_| 0xa5u8).unwrap();
            unsafe { moved.rewind_to(mark) };
            assert_eq!(moved.retained(handle), Some(41));
            assert!(moved.replace_retained(handle, 42));
            assert_eq!(moved.retained(handle), Some(42));
            assert_eq!(other.retained(handle), None);
            assert!(!other.replace_retained(handle, 99));
            moved.reset();
            let replacement = moved.retain(100u64).unwrap();
            assert_eq!(moved.retained(handle), None);
            assert!(!moved.replace_retained(handle, 99));
            assert_eq!(moved.retained(replacement), Some(100));
        });
    }

    struct Counted<'a>(&'a core::cell::Cell<usize>);

    impl Drop for Counted<'_> {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn arena_owned_list_published_slices_survive_edits_and_consumption() {
        let mut budget = Budget::new(2048);
        let mut arena = Arena::new(&mut budget, "list ownership", 2048).unwrap();
        for persistent in [false, true] {
            crate::mem::guard::forbid_alloc(|| {
                let mut list = if persistent {
                    ArenaList::new_persistent(&arena)
                } else {
                    ArenaList::new(&arena)
                };
                for value in [1u64, 2, 3] {
                    list.push(value).unwrap();
                }
                let original = list.as_slice();
                assert_eq!(list.borrowed_slice(), original);
                list.as_mut_slice().unwrap()[0] = 4;
                assert_eq!(original, [1, 2, 3]);
                let edited = list.as_slice();
                list.truncate(1);
                list.push(5).unwrap();
                assert_eq!(edited, [4, 2, 3]);
                let truncated = list.as_slice();
                let consumed = list.into_mut_slice().unwrap();
                consumed[0] = 6;
                assert_eq!(truncated, [4, 5]);
                assert_eq!(original, [1, 2, 3]);
                assert_eq!(consumed, [6, 5]);
            });
            arena.reset();
        }
    }

    #[test]
    fn arena_owned_list_consumption_reuses_unique_storage_and_reports_exhaustion() {
        let mut budget = Budget::new(64);
        let mut arena = Arena::new(&mut budget, "bounded list", 64).unwrap();
        for persistent in [false, true] {
            crate::mem::guard::forbid_alloc(|| {
                let mut list = if persistent {
                    ArenaList::new_persistent(&arena)
                } else {
                    ArenaList::new(&arena)
                };
                list.push(7u64).unwrap();
                let used = arena.used();
                let values = list.into_mut_slice().unwrap();
                assert_eq!(arena.used(), used);
                values[0] = 8;
                assert_eq!(values, [8]);

                let mut list = if persistent {
                    ArenaList::new_persistent(&arena)
                } else {
                    ArenaList::new(&arena)
                };
                list.push(9u64).unwrap();
                let published = list.as_slice();
                assert_eq!(arena.used(), 64);
                assert!(list.as_mut_slice().is_err());
                assert_eq!(published, [9]);
                assert!(list.into_mut_slice().is_err());
                assert_eq!(published, [9]);
            });
            arena.reset();
        }
    }

    #[test]
    fn owned_write_inputs_scoped_arena_values_survive_front_rewinds_and_drop_once() {
        let mut budget = Budget::new(256);
        let mut arena = Arena::new(&mut budget, "scoped owners", 256).unwrap();
        let dropped = core::cell::Cell::new(0);
        crate::mem::guard::forbid_alloc(|| {
            let mark = arena.mark();
            let mut owners = arena
                .alloc_owned_slice_with(3, |_| Counted(&dropped))
                .unwrap();
            arena.alloc_slice_with(32, |_| 7u8).unwrap();
            // SAFETY: only the unused front scratch is discarded; owners live
            // in the separately reserved tail.
            unsafe { arena.rewind_to(mark) };
            arena.alloc_slice_with(64, |_| 9u8).unwrap();
            assert_eq!(owners.len(), 3);
            owners[1] = Counted(&dropped);
            assert_eq!(dropped.get(), 1);
            drop(owners);
            assert_eq!(dropped.get(), 4);
        });
        arena.reset();
        assert_eq!(arena.used(), 0);
    }

    #[test]
    fn owned_write_inputs_scoped_arena_drops_initialized_prefix_after_panic() {
        let mut budget = Budget::new(256);
        let arena = Arena::new(&mut budget, "panic owners", 256).unwrap();
        let dropped = core::cell::Cell::new(0);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _owners = arena
                .alloc_owned_slice_with(4, |index| {
                    assert_ne!(index, 2, "stop initialization");
                    Counted(&dropped)
                })
                .unwrap();
        }));
        assert!(result.is_err());
        assert_eq!(dropped.get(), 2);
    }

    #[test]
    fn owned_write_inputs_scoped_arena_capacity_errors_do_not_initialize_values() {
        let mut budget = Budget::new(64);
        let arena = Arena::new(&mut budget, "bounded owners", 64).unwrap();
        let initialized = core::cell::Cell::new(0);
        crate::mem::guard::forbid_alloc(|| {
            for count in [9, usize::MAX] {
                let error = arena
                    .alloc_owned_slice_with(count, |_| {
                        initialized.set(initialized.get() + 1);
                        0u64
                    })
                    .err()
                    .unwrap();
                assert_eq!(error.capacity, 64);
                assert_eq!(arena.used(), 0);
                assert_eq!(initialized.get(), 0);
            }
        });
    }

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
    fn catalog_snapshot_schema_cache_is_typed_bounded_and_survives_failed_allocations() {
        let mut budget = Budget::new(256);
        let arena = Arena::new(&mut budget, "schema cache", 256).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let first = arena.alloc_interned(7u64, PartialEq::eq).unwrap();
            let used = arena.used();
            let same = arena.alloc_interned(7u64, PartialEq::eq).unwrap();
            assert!(core::ptr::eq(first, same));
            assert_eq!(arena.used(), used);
            assert!(arena.alloc_interned([0u8; 512], PartialEq::eq).is_err());
            assert!(core::ptr::eq(
                first,
                arena.alloc_interned(7u64, PartialEq::eq).unwrap()
            ));
            let different = arena.alloc_interned(9u64, PartialEq::eq).unwrap();
            assert!(!core::ptr::eq(first, different));
            assert!(core::ptr::eq(first, arena.alloc_interned(7u64, PartialEq::eq).unwrap()));
            let typed = arena
                .alloc_interned((1u32, 2u32), |_, _| {
                    panic!("different types cannot share a cache entry")
                })
                .unwrap();
            assert_eq!(*typed, (1, 2));
        });
    }

    #[test]
    fn catalog_snapshot_schema_cache_invalidates_discarded_values_without_reusing_other_allocations()
     {
        let mut budget = Budget::new(256);
        let mut arena = Arena::new(&mut budget, "schema rewind", 256).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mark = arena.mark();
            arena.alloc_interned(7u64, PartialEq::eq).unwrap();
            // No reference to the discarded cached value survives this rewind.
            unsafe { arena.rewind_to(mark) };
            let replacement = arena.alloc([7u8, 0, 0, 0, 0, 0, 0, 0]).unwrap();
            let current = arena.alloc_interned(7u64, PartialEq::eq).unwrap();
            assert_ne!(replacement.as_ptr(), (current as *const u64).cast::<u8>());
            let retained = arena.mark();
            arena.alloc(123u32).unwrap();
            // Both retained references point below the rewind frontier.
            unsafe { arena.rewind_to(retained) };
            assert!(core::ptr::eq(
                current,
                arena.alloc_interned(7u64, PartialEq::eq).unwrap()
            ));
        });
        arena.reset();
        crate::mem::guard::forbid_alloc(|| {
            let replacement = arena.alloc(7u64).unwrap();
            let current = arena.alloc_interned(7u64, PartialEq::eq).unwrap();
            assert!(!core::ptr::eq(replacement, current));
        });
    }

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
