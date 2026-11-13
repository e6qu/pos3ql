//! A bump arena: allocation is a pointer increment, deallocation only
//! happens wholesale via `reset`. Backs per-request state (SQL ASTs,
//! row scratch) that lives exactly as long as one request.
//!
//! Ordinary allocations require Copy because reset does not run destructors.
//! Scoped owned slices release their values before the arena can reset.

use core::cell::Cell;
use core::fmt;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicU64, Ordering};
use std::alloc::Layout;

use super::budget::{Budget, BudgetError};

mod retained_slice;
pub(crate) use retained_slice::{ArenaSliceError, ArenaSliceHandle, FrozenArenaSliceHandle};
mod retained_list;
pub(crate) use retained_list::{RetainedList, RetainedListError};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct ArenaIdentity(u64);

pub struct Arena {
    what: &'static str,
    base: *mut u8,
    capacity: usize,
    offset: Cell<usize>,
    tail_offset: Cell<usize>,
    high_water: Cell<usize>,
    region_active: Cell<bool>,
    borrowed: Option<BorrowedRegion>,
    retained_front: Cell<usize>,
    retention_identity: u64,
    interned: Cell<Option<core::ptr::NonNull<InternedAllocation>>>,
}

#[derive(Clone, Copy)]
struct BorrowedRegion {
    parent: *const Arena,
    offset: usize,
}

#[derive(Clone, Copy)]
struct InternedAllocation {
    value_type: core::any::TypeId,
    length: usize,
    value: *const u8,
    end: usize,
    previous: Option<core::ptr::NonNull<InternedAllocation>>,
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

impl<T> PartialEq for ArenaHandle<T> {
    fn eq(&self, other: &Self) -> bool {
        self.identity == other.identity && self.offset == other.offset
    }
}

impl<T> Eq for ArenaHandle<T> {}

impl<T> Copy for ArenaHandle<T> {}

impl<T> Clone for ArenaHandle<T> {
    fn clone(&self) -> Self {
        *self
    }
}

/// An allocation frontier retained while short-lived work above it is
/// recycled. Moves preserve the allocation identity; reset invalidates it.
#[derive(Clone, Copy)]
pub(crate) struct ArenaMark {
    identity: u64,
    offset: usize,
}

// Owning arenas may move between threads. Borrowed arenas are exposed only
// through a scoped shared borrow; their guard borrows a !Sync parent and cannot
// move between threads. Allocation references stay within their arena borrow.
unsafe impl Send for Arena {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArenaFull {
    pub what: &'static str,
    pub requested: usize,
    pub remaining: usize,
    pub capacity: usize,
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
thread_local! {
    static LAST_ALLOCATION_FAILURE: Cell<Option<(ArenaFull, bool)>> = const { Cell::new(None) };
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
pub(crate) fn take_last_allocation_failure() -> Option<(ArenaFull, bool)> {
    LAST_ALLOCATION_FAILURE.with(|failure| failure.take())
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
    pub(crate) const fn identity(&self) -> ArenaIdentity {
        ArenaIdentity(self.retention_identity)
    }

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
            region_active: Cell::new(false),
            borrowed: None,
            retained_front: Cell::new(0),
            retention_identity: retention_identity(),
            interned: Cell::new(None),
        })
    }

    /// Lends the remaining aligned storage to one scoped arena. The closure
    /// cannot return references into that region; copied handles carry its
    /// distinct identity. Parent front allocations fail during the borrow;
    /// its durable tail can still grow toward the child's shared frontier.
    /// Child retained values pin its front until the complete region ends.
    pub(crate) fn with_remaining_region<R>(
        &self,
        each: impl for<'region> FnOnce(&'region Arena) -> R,
    ) -> Result<R, ArenaFull> {
        if self.region_active.get() {
            return Err(ArenaFull {
                remaining: 0,
                ..self.full(1)
            });
        }
        let offset = self.offset.get();
        let start = offset.next_multiple_of(ARENA_ALIGN);
        let tail = self.allocation_limit();
        if start >= tail {
            return Err(self.full(1));
        }
        let region = ArenaRegion {
            parent: self,
            offset,
            arena: ManuallyDrop::new(Arena {
                what: self.what,
                // SAFETY: this aligned, nonempty range lies between the
                // parent's existing front and tail allocations. Every child
                // allocation publishes its frontier before exposing bytes.
                base: unsafe { self.base.add(start) },
                capacity: tail - start,
                offset: Cell::new(0),
                tail_offset: Cell::new(tail - start),
                high_water: Cell::new(0),
                region_active: Cell::new(false),
                borrowed: Some(BorrowedRegion {
                    parent: self,
                    offset: start,
                }),
                retained_front: Cell::new(0),
                retention_identity: retention_identity(),
                interned: Cell::new(None),
            }),
        };
        self.region_active.set(true);
        self.set_frontier(start);
        Ok(each(&region.arena))
    }

    /// Lends a fixed block of `capacity` bytes from this arena's front to a
    /// child arena whose rewinds never touch the parent's later allocations.
    /// The child and its bytes are ordinary front allocations: they stay
    /// valid until the parent rewinds below them, and are never dropped.
    pub(crate) fn alloc_front_block(&self, capacity: usize) -> Result<&Arena, ArenaFull> {
        let capacity = capacity.next_multiple_of(ARENA_ALIGN);
        if capacity == 0 {
            return Err(self.full(1));
        }
        let layout =
            Layout::from_size_align(capacity, ARENA_ALIGN).map_err(|_| self.full(capacity))?;
        let base = self.alloc_raw(layout)?;
        let header = self.alloc_raw(Layout::new::<Arena>())?.cast::<Arena>();
        // SAFETY: the header slot is fresh, aligned and sized for an Arena.
        // Arena memory is never dropped, so the child never runs Arena::drop
        // and never frees the parent's bytes it borrows.
        unsafe {
            header.write(Arena {
                what: self.what,
                base,
                capacity,
                offset: Cell::new(0),
                tail_offset: Cell::new(capacity),
                high_water: Cell::new(0),
                region_active: Cell::new(false),
                borrowed: None,
                retained_front: Cell::new(0),
                retention_identity: retention_identity(),
                interned: Cell::new(None),
            });
            Ok(&*header)
        }
    }

    /// Lends a fixed block of `capacity` bytes from this arena's persistent
    /// tail to a child arena: the block outlives every front rewind and is
    /// reclaimed only when this arena resets.
    pub(crate) fn alloc_persistent_block(&self, capacity: usize) -> Result<&Arena, ArenaFull> {
        let capacity = capacity.next_multiple_of(ARENA_ALIGN);
        if capacity == 0 {
            return Err(self.full(1));
        }
        let layout =
            Layout::from_size_align(capacity, ARENA_ALIGN).map_err(|_| self.full(capacity))?;
        let base = self.alloc_raw_tail(layout)?;
        let header = self.alloc_raw_tail(Layout::new::<Arena>())?.cast::<Arena>();
        // SAFETY: as for `alloc_front_block`; the tail is never rewound
        // below the block while this arena lives.
        unsafe {
            header.write(Arena {
                what: self.what,
                base,
                capacity,
                offset: Cell::new(0),
                tail_offset: Cell::new(capacity),
                high_water: Cell::new(0),
                region_active: Cell::new(false),
                borrowed: None,
                retained_front: Cell::new(0),
                retention_identity: retention_identity(),
                interned: Cell::new(None),
            });
            Ok(&*header)
        }
    }

    /// Bytes still available to front allocations.
    pub(crate) fn remaining(&self) -> usize {
        self.allocation_limit().saturating_sub(self.offset.get())
    }

    /// Reclaims a scoped workspace while retaining only its encoded output.
    /// The output is moved into this arena, including when this is a borrowed
    /// row arena; temporary child-persistent values do not pin its workspace.
    pub(crate) fn with_remaining_bytes<E>(
        &self,
        each: impl for<'region> FnOnce(&'region Arena) -> Result<&'region [u8], E>,
    ) -> Result<Result<&[u8], E>, ArenaFull> {
        let output = self.with_remaining_region(|region| {
            each(region).map(|bytes| (bytes.as_ptr(), bytes.len()))
        })?;
        let (source, len) = match output {
            Ok(output) => output,
            Err(error) => return Ok(Err(error)),
        };
        let layout = Layout::array::<u8>(len).map_err(|_| self.full(len))?;
        let destination = self.alloc_raw(layout)?;
        // SAFETY: ending the region releases its allocation frontier, not the
        // backing allocation or its initialized bytes. Reserve the output
        // without initializing over its source; memmove permits overlap as
        // these bytes compact into the parent's newly available front. No
        // reference into the old child escapes this boundary.
        unsafe {
            core::ptr::copy(source, destination, len);
            Ok(Ok(core::slice::from_raw_parts(destination, len)))
        }
    }

    /// State that must survive scoped row regions belongs to the owning
    /// arena's durable tail, whose frontier is shared with every active child.
    pub(crate) fn retention_owner(&self) -> &Self {
        match self.borrowed {
            Some(region) => {
                // SAFETY: a region's guard borrows its parent until this arena
                // becomes inaccessible; the returned borrow cannot outlive it.
                unsafe { &*region.parent }.retention_owner()
            }
            None => self,
        }
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

    /// Shares actual-length immutable slices within the same rewind boundary.
    pub(crate) fn alloc_interned_slice<T: Copy + PartialEq + Send + Sync + 'static>(
        &self,
        values: &[T],
    ) -> Result<&[T], ArenaFull> {
        if values.is_empty() {
            return Ok(&[]);
        }
        let mut cursor = self.interned.get();
        while let Some(previous) = cursor {
            // Cache entries remain initialized until rewind removes their links.
            let previous = unsafe { previous.as_ref() };
            if previous.value_type == core::any::TypeId::of::<[T]>()
                && previous.length == values.len()
            {
                let prior = unsafe {
                    core::slice::from_raw_parts(previous.value.cast::<T>(), previous.length)
                };
                if prior == values {
                    return Ok(prior);
                }
            }
            cursor = previous.previous;
        }
        let elements = Layout::array::<T>(values.len()).map_err(|_| self.full(usize::MAX))?;
        let (layout, elements_offset) = Layout::new::<InternedAllocation>()
            .extend(elements)
            .map_err(|_| self.full(usize::MAX))?;
        let allocation = self.alloc_raw(layout.pad_to_align())?;
        let identity = allocation.cast::<InternedAllocation>();
        let elements = unsafe { allocation.add(elements_offset).cast::<T>() };
        // One disjoint checked allocation contains both the typed elements and
        // their cache identity. Publish the link only after both are initialized.
        unsafe {
            core::ptr::copy_nonoverlapping(values.as_ptr(), elements, values.len());
            identity.write(InternedAllocation {
                value_type: core::any::TypeId::of::<[T]>(),
                length: values.len(),
                value: elements.cast(),
                end: self.offset.get(),
                previous: self.interned.get(),
            });
            self.interned
                .set(Some(core::ptr::NonNull::new_unchecked(identity)));
            Ok(core::slice::from_raw_parts(elements, values.len()))
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

    /// A fixed `bytes` workspace carved from the owning arena's durable tail,
    /// for statement state built by code that allocates through an ordinary
    /// arena (an analysis run part-way through a statement). Its allocations,
    /// including its own marks and rewinds, never touch this arena's front,
    /// so per-row rewinds cannot reclaim them; the owner's reset does.
    pub(crate) fn persistent_workspace(
        &self,
        what: &'static str,
        bytes: usize,
    ) -> Result<&Arena, ArenaFull> {
        let owner = self.retention_owner();
        let capacity = bytes.next_multiple_of(ARENA_ALIGN);
        let layout =
            Layout::from_size_align(capacity, ARENA_ALIGN).map_err(|_| owner.full(capacity))?;
        let base = owner.alloc_raw_tail(layout)?;
        let header = owner
            .alloc_raw_tail(Layout::new::<Arena>())?
            .cast::<Arena>();
        // SAFETY: both ranges are disjoint, aligned tail allocations of the
        // owner, live until its `reset`, which takes `&mut self` and so ends
        // every borrow returned here. The header is never dropped: its
        // storage belongs to the owner and must not be freed through it.
        unsafe {
            header.write(Arena {
                what,
                base,
                capacity,
                offset: Cell::new(0),
                tail_offset: Cell::new(capacity),
                high_water: Cell::new(0),
                region_active: Cell::new(false),
                borrowed: None,
                retained_front: Cell::new(0),
                retention_identity: retention_identity(),
                interned: Cell::new(None),
            });
            Ok(&*header)
        }
    }

    /// Rewinds the arena. Requires `&mut self`, so the borrow checker
    /// guarantees no allocation handed out earlier is still alive.
    pub fn reset(&mut self) {
        self.retained_front.set(0);
        self.recycle_front();
        self.tail_offset.set(self.capacity);
        self.retention_identity = retention_identity();
    }

    /// Reclaims temporary parsing and row values while retained scan handles
    /// keep their identity and bytes. Live references prevent this mutable borrow.
    pub(crate) fn recycle_front(&mut self) {
        assert!(!self.region_active.get(), "arena region is still borrowed");
        self.interned.set(None);
        self.set_frontier(self.retained_front.get());
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
            identity: self.retention_identity,
            offset: self.offset.get(),
        }
    }

    /// Recycles front allocations after `mark`. Owning-arena tail allocations
    /// stay live; a borrowed region also preserves the prefix containing its
    /// retained values until that region ends.
    /// An unchanged frontier needs no rewind, even while a child region lives.
    ///
    /// # Safety
    ///
    /// No reference into the actually discarded suffix may be used afterward.
    /// This is reserved for executor choke points that encode or emit every
    /// per-row value before rewinding; long-lived AST/scope state is allocated
    /// below the mark.
    pub(crate) unsafe fn rewind_to(&self, mark: ArenaMark) {
        assert!(
            mark.identity != 0 && mark.identity == self.retention_identity,
            "arena mark was reset or replaced"
        );
        assert!(
            mark.offset <= self.offset.get(),
            "arena mark is ahead of the frontier"
        );
        let offset = mark.offset.max(self.retained_front.get());
        if offset == self.offset.get() {
            return;
        }
        assert!(!self.region_active.get(), "arena region is still borrowed");
        while let Some(cached) = self.interned.get() {
            // Entries remain initialized until the frontier moves below them.
            let cached = unsafe { cached.as_ref() };
            if cached.end <= offset {
                break;
            }
            self.interned.set(cached.previous);
        }
        self.set_frontier(offset);
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
        if self.region_active.get() {
            return Err(ArenaFull {
                remaining: 0,
                ..self.full(layout.size())
            });
        }
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
        if end > self.allocation_limit() {
            return Err(self.full(layout.size()));
        }
        self.set_frontier(end);
        Ok(unsafe { self.base.add(start) })
    }

    fn alloc_raw_tail(&self, layout: Layout) -> Result<*mut u8, ArenaFull> {
        if self.borrowed.is_some() {
            // Borrowed regions cannot put temporary retained values into their
            // owner's durable tail. Allocate them forward and pin that prefix
            // against local rewinds; the region guard releases it wholesale.
            let pointer = self.alloc_raw(layout)?;
            self.retained_front.set(self.offset.get());
            return Ok(pointer);
        }
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
        self.record_high_water();
        Ok(unsafe { self.base.add(start) })
    }

    fn full(&self, requested: usize) -> ArenaFull {
        let error = ArenaFull {
            what: self.what,
            requested,
            remaining: self.allocation_limit().saturating_sub(self.offset.get()),
            capacity: self.capacity,
        };
        #[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
        LAST_ALLOCATION_FAILURE
            .with(|failure| failure.set(Some((error, self.region_active.get()))));
        error
    }

    fn allocation_limit(&self) -> usize {
        match self.borrowed {
            Some(region) => {
                // SAFETY: the enclosing guard pins this parent and excludes
                // its front allocations. Only its durable tail may advance.
                unsafe { &*region.parent }
                    .allocation_limit()
                    .saturating_sub(region.offset)
                    .min(self.tail_offset.get())
            }
            None => self.tail_offset.get(),
        }
    }

    fn set_frontier(&self, offset: usize) {
        self.offset.set(offset);
        self.record_high_water();
        if let Some(region) = self.borrowed {
            // SAFETY: this live child exclusively owns its parent's front.
            // Publish before any allocation returns, preventing the owning
            // arena's durable tail from overlapping initialized child bytes.
            unsafe { &*region.parent }.set_frontier(region.offset + offset);
        }
    }

    fn record_high_water(&self) {
        self.high_water.set(self.high_water.get().max(self.used()));
    }
}

struct ArenaRegion<'parent> {
    parent: &'parent Arena,
    offset: usize,
    // This arena borrows its allocation. Never run Arena::drop on it.
    arena: ManuallyDrop<Arena>,
}

impl Drop for ArenaRegion<'_> {
    fn drop(&mut self) {
        self.parent.set_frontier(self.offset);
        self.parent.region_active.set(false);
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

    /// Drops the text past byte `len`, which must be a character boundary.
    pub(crate) fn truncate(&mut self, len: usize) {
        assert!(self.as_str().is_char_boundary(len));
        self.bytes.truncate(len);
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
    try_stable_sort_via(arena, items, |a, b| Ok(cmp(a, b)), |full| full)
}

/// [`stable_sort_via`] with a fallible comparison: the first comparison
/// error stops the sort and is returned, leaving `items` in their original
/// order; `arena_full` reports exhaustion of the staging space.
pub fn try_stable_sort_via<T, E>(
    arena: &Arena,
    items: &mut [T],
    mut cmp: impl FnMut(&T, &T) -> Result<core::cmp::Ordering, E>,
    arena_full: impl FnOnce(ArenaFull) -> E,
) -> Result<(), E> {
    let n = items.len();
    if n < 2 {
        return Ok(());
    }
    let (perm, visited) = match arena.alloc_slice_with(n, |i| i as u32).and_then(|perm| {
        arena
            .alloc_slice_with(n, |_| false)
            .map(|visited| (perm, visited))
    }) {
        Ok(staged) => staged,
        Err(full) => return Err(arena_full(full)),
    };
    crate::util::try_sort_unstable_by(perm, |&a, &b| {
        Ok(cmp(&items[a as usize], &items[b as usize])?.then_with(|| a.cmp(&b)))
    })?;
    // Apply the permutation by cycles: each element moves at most once.
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
    fn scoped_encoded_outputs_compact_overlapping_bytes_and_preserve_durable_effects() {
        let mut budget = Budget::new(1024);
        let arena = Arena::new(&mut budget, "encoded result", 1024).unwrap();
        let prefix = arena.alloc_slice_copy(&[0xa5u8; 3]).unwrap();
        let source = Cell::new(0usize);
        let durable = Cell::new(None);
        crate::mem::guard::forbid_alloc(|| {
            let output = arena
                .with_remaining_bytes::<()>(|body| {
                    let bytes = body.alloc_slice_with(128, |index| index as u8).unwrap();
                    durable.set(Some(arena.retain(71u64).unwrap()));
                    source.set(bytes[5..69].as_ptr() as usize);
                    Ok(&bytes[5..69])
                })
                .unwrap()
                .unwrap();
            let destination = output.as_ptr() as usize;
            assert!(destination < source.get() && source.get() < destination + output.len());
            let expected: [u8; 64] = core::array::from_fn(|index| (index + 5) as u8);
            assert_eq!(output, &expected);
            assert_eq!(prefix, &[0xa5; 3]);
            assert_eq!(arena.retained(durable.get().unwrap()), Some(71));
            assert_eq!(arena.used(), 3 + 64 + 8);
        });
    }

    #[test]
    fn scoped_encoded_outputs_reclaim_nested_borrowed_plans_without_promoting_rows() {
        let mut budget = Budget::new(2048);
        let arena = Arena::new(&mut budget, "nested result", 2048).unwrap();
        let effect = arena.retain(83u64).unwrap();
        let before = arena.used();
        crate::mem::guard::forbid_alloc(|| {
            for _ in 0..4 {
                arena
                    .with_remaining_region(|row| {
                        let prefix = row.alloc_slice_copy(&[17u8; 3]).unwrap();
                        let output = row
                            .with_remaining_bytes::<()>(|body| {
                                body.alloc_persistent_slice_with(128, |_| 0xadu8).unwrap();
                                let nested = body
                                    .with_remaining_bytes::<()>(|inner| {
                                        inner.alloc_persistent_slice_with(96, |_| 0xbcu8).unwrap();
                                        assert_eq!(arena.retained(effect), Some(83));
                                        Ok(inner.alloc_slice_copy(&[31u8; 48]).unwrap())
                                    })
                                    .unwrap()
                                    .unwrap();
                                Ok(nested)
                            })
                            .unwrap()
                            .unwrap();
                        assert_eq!(output, &[31; 48]);
                        assert_eq!(prefix, &[17; 3]);
                        assert_eq!(row.used(), 3 + 48);
                    })
                    .unwrap();
                assert_eq!(arena.used(), before);
            }
            assert_eq!(arena.retained(effect), Some(83));
        });
    }

    #[test]
    fn scoped_encoded_outputs_restore_owners_after_errors_unwind_and_capacity_failure() {
        static TOO_LARGE: [u8; 2048] = [0x7e; 2048];
        let mut budget = Budget::new(1024);
        let arena = Arena::new(&mut budget, "failed result", 1024).unwrap();
        let prefix = arena.alloc(19u64).unwrap();
        let before = arena.used();
        let effect = Cell::new(None);
        let failed = arena
            .with_remaining_bytes(|body| {
                body.alloc_persistent_slice_with(128, |_| 0xa5u8).unwrap();
                effect.set(Some(arena.retain(97u64).unwrap()));
                Err("body failed")
            })
            .unwrap();
        assert_eq!(failed, Err("body failed"));
        assert_eq!(arena.used(), before + 8);
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<Result<&[u8], ()>, _> = arena.with_remaining_bytes(|body| {
                body.alloc_persistent_slice_with(128, |_| 0xbcu8).unwrap();
                panic!("stop encoded result");
            });
        }));
        assert!(unwound.is_err());
        assert_eq!(arena.used(), before + 8);
        let failed = arena.with_remaining_bytes::<()>(|_| Ok(&TOO_LARGE));
        assert_eq!(failed.unwrap_err().what, "failed result");
        assert_eq!(arena.used(), before + 8);
        let output = arena
            .with_remaining_bytes::<()>(|body| Ok(body.alloc_slice_copy(&[23u8; 8]).unwrap()))
            .unwrap()
            .unwrap();
        assert_eq!(output, &[23; 8]);
        assert_eq!(*prefix, 19);
        assert_eq!(arena.retained(effect.get().unwrap()), Some(97));
    }

    #[test]
    fn cursor_owned_query_regions_reuse_storage_without_aliasing_or_heap_allocation() {
        let mut budget = Budget::new(256);
        let arena = Arena::new(&mut budget, "query regions", 256).unwrap();
        let original = arena.alloc(7u8).unwrap();
        let saved = arena.retain(41u64).unwrap();
        let previous_used = arena.used();
        crate::mem::guard::forbid_alloc(|| {
            let stale = arena
                .with_remaining_region(|region| {
                    assert_eq!(region.capacity(), 232);
                    let aligned = region.alloc(42u128).unwrap();
                    assert_eq!((aligned as *const u128 as usize) % ARENA_ALIGN, 0);
                    let handle = region.retain(43u64).unwrap();
                    assert_eq!(region.retained(handle), Some(43));
                    assert_eq!(region.retained(saved), None);
                    assert_eq!(*original, 7);
                    assert_eq!(arena.retained(saved), Some(41));
                    for error in [
                        arena.alloc(9u8).unwrap_err(),
                        arena.alloc(()).unwrap_err(),
                        arena.with_remaining_region(|_| ()).unwrap_err(),
                    ] {
                        assert_eq!(error.what, "query regions");
                        assert_eq!(error.remaining, 0);
                    }
                    assert_eq!(*aligned, 42);
                    handle
                })
                .unwrap();
            assert_eq!(arena.used(), previous_used);
            assert_eq!(arena.high_water(), 48);
            assert_eq!(arena.retained(stale), None);
            assert!(!arena.replace_retained(stale, 0));
            arena
                .with_remaining_region(|region| {
                    assert_eq!(region.retained(stale), None);
                    assert!(!region.replace_retained(stale, 0));
                    let mark = region.mark();
                    region.alloc(11u64).unwrap();
                    let front = region.used();
                    region
                        .with_remaining_region(|nested| {
                            assert_eq!(*nested.alloc(99u128).unwrap(), 99);
                        })
                        .unwrap();
                    assert_eq!(region.used(), front);
                    // No reference into the discarded front allocation survives.
                    unsafe { region.rewind_to(mark) };
                    let bytes = region
                        .alloc_slice_with(region.capacity(), |_| 0xa5u8)
                        .unwrap();
                    assert_eq!(bytes[bytes.len() - 1], 0xa5);
                })
                .unwrap();
            assert_eq!(arena.used(), previous_used);
            assert_eq!(arena.high_water(), arena.capacity());
            assert_eq!(*original, 7);
            assert_eq!(arena.retained(saved), Some(41));
            assert_eq!(*arena.alloc(19u64).unwrap(), 19);
        });
    }

    #[test]
    fn cursor_owned_query_regions_share_durable_tail_without_reusing_pinned_fronts() {
        let mut budget = Budget::new(512);
        let arena = Arena::new(&mut budget, "shared row and durable effects", 512).unwrap();
        let prefix = arena.alloc(7u8).unwrap();
        let initial = arena.used();
        crate::mem::guard::forbid_alloc(|| {
            let (first, second, stale) = arena
                .with_remaining_region(|row| {
                    assert!(core::ptr::eq(row.retention_owner(), &arena));
                    let mark = row.mark();
                    row.alloc([3u8; 16]).unwrap();
                    let retained = row.retain(41u64).unwrap();
                    let pinned = row.used();
                    let first = arena.retain(51u64).unwrap();
                    let second = row
                        .with_remaining_region(|nested| {
                            assert!(core::ptr::eq(nested.retention_owner(), &arena));
                            let value = nested.alloc(71u128).unwrap();
                            let second = arena.retain(61u128).unwrap();
                            assert!(row.retain(0u8).is_err());
                            assert_eq!(*value, 71);
                            second
                        })
                        .unwrap();
                    assert_eq!(row.used(), pinned);
                    // Retained values pin this prefix; no discarded suffix remains.
                    unsafe { row.rewind_to(mark) };
                    assert_eq!(row.used(), pinned);
                    assert_eq!(row.retained(retained), Some(41));
                    assert!(
                        row.alloc_slice_with(row.capacity(), |_| -> u8 {
                            panic!("cannot cross durable tail")
                        })
                        .is_err()
                    );
                    assert_eq!(arena.retained(first), Some(51));
                    assert_eq!(arena.retained(second), Some(61));
                    (first, second, retained)
                })
                .unwrap();
            assert_eq!(*prefix, 7);
            assert!(arena.used() > initial);
            assert_eq!(arena.retained(first), Some(51));
            assert_eq!(arena.retained(second), Some(61));
            assert_eq!(arena.retained(stale), None);
            arena
                .with_remaining_region(|row| {
                    row.alloc_slice_with(256, |_| 0xa5u8).unwrap();
                    assert_eq!(row.retained(stale), None);
                    assert_eq!(arena.retained(first), Some(51));
                    assert_eq!(arena.retained(second), Some(61));
                })
                .unwrap();
            assert!(arena.high_water() <= arena.capacity());
        });
    }

    #[test]
    fn cursor_owned_query_regions_count_actual_overlapping_usage_and_preserve_tail_on_unwind() {
        let mut budget = Budget::new(512);
        let arena = Arena::new(&mut budget, "row peak and durable tail", 512).unwrap();
        let durable = Cell::new(None);
        crate::mem::guard::forbid_alloc(|| {
            arena
                .with_remaining_region(|row| {
                    let mark = row.mark();
                    row.alloc([0u8; 400]).unwrap();
                    unsafe { row.rewind_to(mark) };
                    durable.set(Some(arena.retain([0xa5u8; 192]).unwrap()));
                    row.alloc([0u8; 16]).unwrap();
                })
                .unwrap();
            assert_eq!(arena.used(), 192);
            assert_eq!(arena.high_water(), 400);
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<(), ArenaFull> = arena.with_remaining_region(|row| {
                row.alloc([7u8; 16]).unwrap();
                assert_eq!(arena.retained(durable.get().unwrap()), Some([0xa5u8; 192]));
                panic!("stop borrowed row after durable effects");
            });
        }));
        assert!(result.is_err());
        assert_eq!(arena.used(), 192);
        assert_eq!(arena.high_water(), 400);
        assert_eq!(arena.retained(durable.get().unwrap()), Some([0xa5u8; 192]));
        arena.alloc([0u8; 320]).unwrap();
        assert_eq!(arena.high_water(), 512);
        assert_eq!(arena.retained(durable.get().unwrap()), Some([0xa5u8; 192]));
    }

    #[test]
    fn cursor_owned_query_regions_restore_frontiers_and_drop_values_after_unwind() {
        let mut budget = Budget::new(256);
        let mut arena = Arena::new(&mut budget, "unwinding region", 256).unwrap();
        arena.alloc(7u8).unwrap();
        let saved = arena.retain(8u64).unwrap();
        let previous_used = arena.used();
        let dropped = Cell::new(0);
        let stale = Cell::new(None);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<(), ArenaFull> = arena.with_remaining_region(|region| {
                stale.set(Some(region.retain(9u64).unwrap()));
                let _owned = region
                    .alloc_owned_slice_with(2, |_| Counted(&dropped))
                    .unwrap();
                region.alloc(10u128).unwrap();
                panic!("stop scoped query");
            });
        }));
        assert!(result.is_err());
        assert_eq!(dropped.get(), 2);
        assert_eq!(arena.used(), previous_used);
        assert!(arena.high_water() > previous_used);
        assert_eq!(arena.retained(saved), Some(8));
        assert_eq!(arena.retained(stale.get().unwrap()), None);
        arena
            .with_remaining_region(|region| {
                assert_eq!(*region.alloc(11u128).unwrap(), 11);
            })
            .unwrap();
        arena.reset();
        assert_eq!(arena.used(), 0);
        assert_eq!(*arena.alloc([0u8; 256]).unwrap(), [0u8; 256]);
    }

    #[test]
    fn cursor_owned_query_regions_reject_empty_aligned_capacity_without_consuming_storage() {
        let mut budget = Budget::new(64);
        let arena = Arena::new(&mut budget, "small query region", 64).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            arena.alloc([0u8; 63]).unwrap();
            let error = arena
                .with_remaining_region(|_| panic!("no aligned storage"))
                .unwrap_err();
            assert_eq!(error.what, "small query region");
            assert_eq!(error.remaining, 1);
            assert_eq!(arena.used(), 63);
            assert_eq!(arena.high_water(), 63);
            arena.alloc(1u8).unwrap();
            let error = arena
                .with_remaining_region(|_| panic!("no storage"))
                .unwrap_err();
            assert_eq!(error.remaining, 0);
            assert_eq!(arena.used(), 64);
        });
    }

    #[test]
    fn cursor_owned_query_scratch_marks_follow_allocations_across_moves() {
        let mut budget = Budget::new(8192);
        let original = Arena::new(&mut budget, "movable query scratch", 4096).unwrap();
        let other = Arena::new(&mut budget, "other query scratch", 4096).unwrap();
        let value = original.retain(7u64).unwrap();
        let stable_used = original.used();
        let mark = original.mark();
        original.alloc_slice_with(128, |_| 3u8).unwrap();
        let mut moved = original;
        crate::mem::guard::forbid_alloc(|| {
            unsafe { moved.rewind_to(mark) };
            assert_eq!(moved.used(), stable_used);
            assert_eq!(moved.retained(value), Some(7));
        });
        let before_child = moved.mark();
        moved
            .with_remaining_region(|child| {
                let held = child.alloc_slice_with(32, |_| 0xa5_u8).unwrap();
                let used = moved.used();
                let unchanged = moved.mark();
                crate::mem::guard::forbid_alloc(|| unsafe {
                    moved.rewind_to(unchanged);
                });
                let error = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
                    moved.rewind_to(before_child);
                }));
                assert!(error.is_err());
                assert_eq!(moved.used(), used);
                assert!(held.iter().all(|byte| *byte == 0xa5));
            })
            .unwrap();
        assert_eq!(moved.used(), stable_used);
        let wrong = other.mark();
        let error = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            moved.rewind_to(wrong);
        }));
        assert!(error.is_err());
        let stale = moved.mark();
        moved.reset();
        let error = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            moved.rewind_to(stale);
        }));
        assert!(error.is_err());
    }

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
    fn interned_slices_are_actual_length_typed_and_rewind_with_their_values() {
        let mut budget = Budget::new(1024);
        let arena = Arena::new(&mut budget, "actual schema columns", 1024).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let first = arena.alloc_interned_slice(&[1u32, 2]).unwrap();
            let initial_used = arena.used();
            assert!(core::ptr::eq(
                first,
                arena.alloc_interned_slice(&[1u32, 2]).unwrap()
            ));
            assert_eq!(arena.used(), initial_used);
            let short = arena.alloc_interned_slice(&[1u32]).unwrap();
            assert_eq!(short, &[1]);
            assert!(!core::ptr::eq(first.as_ptr(), short.as_ptr()));
            let scalar = arena.alloc(1u32).unwrap();
            assert!(!core::ptr::eq(short.as_ptr(), scalar));
            let mark = arena.mark();
            assert_eq!(
                arena.alloc_interned_slice(&[9u32, 10, 11]).unwrap(),
                &[9, 10, 11]
            );
            // No reference to the discarded suffix is used after this point.
            unsafe { arena.rewind_to(mark) };
            let reused = arena.alloc_slice_copy(&[55u32, 56, 57, 58]).unwrap();
            assert_eq!(
                arena.alloc_interned_slice(&[9u32, 10, 11]).unwrap(),
                &[9, 10, 11]
            );
            assert_eq!(reused, &[55, 56, 57, 58]);
            assert!(core::ptr::eq(
                first,
                arena.alloc_interned_slice(&[1u32, 2]).unwrap()
            ));
            assert!(arena.alloc_interned_slice(&[0u32; 512]).is_err());
            assert_eq!(arena.alloc_interned_slice(&[1u32, 2]).unwrap(), &[1, 2]);
        });
    }

    #[test]
    fn catalog_snapshot_schema_cache_is_typed_bounded_and_survives_failed_allocations() {
        let mut budget = Budget::new(256);
        let arena = Arena::new(&mut budget, "schema cache", 256).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let first = arena.alloc_interned_slice(&[7u64]).unwrap();
            let used = arena.used();
            let same = arena.alloc_interned_slice(&[7u64]).unwrap();
            assert!(core::ptr::eq(first, same));
            assert_eq!(arena.used(), used);
            assert!(arena.alloc_interned_slice(&[0u8; 512]).is_err());
            assert!(core::ptr::eq(
                first,
                arena.alloc_interned_slice(&[7u64]).unwrap()
            ));
            let different = arena.alloc_interned_slice(&[9u64]).unwrap();
            assert!(!core::ptr::eq(first, different));
            assert!(core::ptr::eq(
                first,
                arena.alloc_interned_slice(&[7u64]).unwrap()
            ));
            #[derive(Clone, Copy)]
            struct DifferentType(u64);
            impl PartialEq for DifferentType {
                fn eq(&self, _: &Self) -> bool {
                    panic!("different types cannot share a cache entry")
                }
            }
            let typed = arena.alloc_interned_slice(&[DifferentType(7)]).unwrap();
            assert_eq!(typed[0].0, 7);
            assert_ne!(first.as_ptr().cast::<u8>(), typed.as_ptr().cast::<u8>());
        });
    }

    #[test]
    fn catalog_snapshot_schema_cache_invalidates_discarded_values_without_reusing_other_allocations()
     {
        let mut budget = Budget::new(256);
        let mut arena = Arena::new(&mut budget, "schema rewind", 256).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mark = arena.mark();
            arena.alloc_interned_slice(&[7u64]).unwrap();
            // No reference to the discarded cached value survives this rewind.
            unsafe { arena.rewind_to(mark) };
            let replacement = arena.alloc([7u8, 0, 0, 0, 0, 0, 0, 0]).unwrap();
            let current = arena.alloc_interned_slice(&[7u64]).unwrap();
            assert_ne!(replacement.as_ptr(), current.as_ptr().cast::<u8>());
            let retained = arena.mark();
            arena.alloc(123u32).unwrap();
            // Both retained references point below the rewind frontier.
            unsafe { arena.rewind_to(retained) };
            assert!(core::ptr::eq(
                current,
                arena.alloc_interned_slice(&[7u64]).unwrap()
            ));
        });
        arena.reset();
        crate::mem::guard::forbid_alloc(|| {
            let replacement = arena.alloc(7u64).unwrap();
            let current = arena.alloc_interned_slice(&[7u64]).unwrap();
            assert!(!core::ptr::eq(replacement, current.as_ptr()));
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
