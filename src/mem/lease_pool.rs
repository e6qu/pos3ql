//! Typed, uniquely mutable or retained immutable ranges in fixed startup backing.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::{ManuallyDrop, MaybeUninit, align_of, size_of};
use core::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicUsize, Ordering};
use {crate::mem::mutex::Mutex, std::sync::Arc};

use super::budget::{Budget, BudgetError};
use super::fixed_vec::FixedVec;

const ALIGNMENT: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Purpose {
    pool: &'static str,
    bytes: &'static str,
    leases: &'static str,
    ranges: &'static str,
}

impl Purpose {
    pub(crate) const EXECUTION_CONTROL: Self = Self {
        pool: "execution control pool",
        bytes: "execution_control_bytes",
        leases: "execution_control_leases",
        ranges: "execution control free ranges",
    };
    pub(crate) const ROW_WORK: Self = Self {
        pool: "row work pool",
        bytes: "row_work_bytes",
        leases: "row_work_leases",
        ranges: "row work free ranges",
    };
    pub(crate) const TABLE_METADATA: Self = Self {
        pool: "table metadata pool",
        bytes: "table_metadata_bytes",
        leases: "table_metadata_leases",
        ranges: "table metadata free ranges",
    };
    pub(crate) const STATISTICS: Self = Self {
        pool: "statistics pool",
        bytes: "statistics_bytes",
        leases: "statistics_leases",
        ranges: "statistics free ranges",
    };
    pub(crate) const RESULT_SPOOL: Self = Self {
        pool: "result spool pool",
        bytes: "result_spool_bytes",
        leases: "result_spool_leases",
        ranges: "result spool free ranges",
    };
}

#[repr(C, align(64))]
struct Block {
    _bytes: UnsafeCell<[MaybeUninit<u8>; ALIGNMENT]>,
}

#[derive(Clone, Copy)]
struct Range {
    start: usize,
    bytes: usize,
}

struct Descriptor {
    generation: u64,
    range: Option<Range>,
}
struct State {
    descriptors: Box<[Descriptor]>,
    free: FixedVec<Range>,
}

/// Allocator locks protect only extent ownership, never payload access or drops.
/// A descriptor remains occupied while the final payload destructor runs.
pub(crate) struct LeasePool {
    purpose: Purpose,
    backing: Box<[Block]>,
    usable_bytes: usize,
    references: Box<[AtomicUsize]>,
    state: Mutex<State>,
}

// Payload access is restricted to typed owners of disjoint occupied ranges.
// Those owners carry T's Send/Sync constraints; the pool exposes no raw handles.
unsafe impl Send for LeasePool {}
unsafe impl Sync for LeasePool {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LeaseError {
    pub purpose: Purpose,
    pub requested_bytes: usize,
    pub free_bytes: usize,
    pub largest_range_bytes: usize,
    pub available_leases: usize,
}

impl core::fmt::Display for LeaseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{} exhausted ({}, {}): requested {} bytes, {} free bytes, largest range {} bytes, {} available leases",
            self.purpose.pool,
            self.purpose.bytes,
            self.purpose.leases,
            self.requested_bytes,
            self.free_bytes,
            self.largest_range_bytes,
            self.available_leases
        )
    }
}
impl std::error::Error for LeaseError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LeaseSetupError {
    Budget(BudgetError),
    Alignment { setting: &'static str, bytes: usize },
    CapacityOverflow { what: &'static str },
}
impl From<BudgetError> for LeaseSetupError {
    fn from(error: BudgetError) -> Self {
        Self::Budget(error)
    }
}
impl core::fmt::Display for LeaseSetupError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Budget(error) => core::fmt::Display::fmt(error, f),
            Self::Alignment { setting, bytes } => {
                write!(f, "{setting} must be a multiple of 64 bytes (got {bytes})")
            }
            Self::CapacityOverflow { what } => {
                write!(f, "{what} exceeds the supported lease pool memory layout")
            }
        }
    }
}
impl std::error::Error for LeaseSetupError {}

#[derive(Clone, Copy)]
struct Reservation {
    slot: usize,
    generation: u64,
    range: Range,
}

/// Only initialized values are exposed. Growth moves them without cloning or
/// dropping them; failure preserves the original range and initialized prefix.
pub(crate) struct MutableSlice<T> {
    pool: Arc<LeasePool>,
    reservation: Option<Reservation>,
    len: usize,
    capacity: usize,
    value: PhantomData<T>,
}

/// References returned by Deref borrow this owner. Clone retains the initialized
/// allocation, including non-Copy values, without reserving bytes or descriptors.
pub struct SharedSlice<T> {
    pool: Option<Arc<LeasePool>>,
    reservation: Option<Reservation>,
    len: usize,
    // Shared owners can reach T concurrently, just as Arc<T> owners can.
    value: PhantomData<Arc<T>>,
}

impl<T> core::fmt::Debug for MutableSlice<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MutableSlice")
            .field("len", &self.len)
            .field("capacity", &self.capacity)
            .finish_non_exhaustive()
    }
}
impl<T> core::fmt::Debug for SharedSlice<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SharedSlice")
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

fn rounded(bytes: usize) -> Option<usize> {
    bytes
        .checked_add(ALIGNMENT - 1)
        .map(|n| n & !(ALIGNMENT - 1))
}

impl LeasePool {
    pub(crate) fn budget_bytes(bytes: usize, leases: usize) -> usize {
        size_of::<Self>()
            .saturating_add(2 * size_of::<usize>())
            .saturating_add(rounded(bytes).unwrap_or(usize::MAX))
            .saturating_add(
                leases.saturating_mul(size_of::<Descriptor>() + size_of::<AtomicUsize>()),
            )
            .saturating_add(leases.saturating_add(1).saturating_mul(size_of::<Range>()))
    }

    pub(crate) fn new(
        budget: &mut Budget,
        purpose: Purpose,
        bytes: usize,
        leases: usize,
    ) -> Result<Arc<Self>, LeaseSetupError> {
        if !bytes.is_multiple_of(ALIGNMENT) {
            return Err(LeaseSetupError::Alignment {
                setting: purpose.bytes,
                bytes,
            });
        }
        let overflow = |what| LeaseSetupError::CapacityOverflow { what };
        let backing_bytes = rounded(bytes).ok_or_else(|| overflow(purpose.bytes))?;
        let free_slots = leases
            .checked_add(1)
            .ok_or_else(|| overflow(purpose.leases))?;
        std::alloc::Layout::array::<Block>(backing_bytes / ALIGNMENT)
            .map_err(|_| overflow(purpose.bytes))?;
        std::alloc::Layout::array::<Descriptor>(leases).map_err(|_| overflow(purpose.leases))?;
        std::alloc::Layout::array::<AtomicUsize>(leases).map_err(|_| overflow(purpose.leases))?;
        std::alloc::Layout::array::<Range>(free_slots).map_err(|_| overflow(purpose.ranges))?;
        let descriptors_bytes = leases
            .checked_mul(size_of::<Descriptor>() + size_of::<AtomicUsize>())
            .ok_or_else(|| overflow(purpose.leases))?;
        let control_bytes = (size_of::<Self>() + 2 * size_of::<usize>())
            .checked_add(backing_bytes)
            .and_then(|n| n.checked_add(descriptors_bytes))
            .ok_or_else(|| overflow(purpose.pool))?;
        let free_bytes = free_slots
            .checked_mul(size_of::<Range>())
            .ok_or_else(|| overflow(purpose.ranges))?;
        control_bytes
            .checked_add(free_bytes)
            .ok_or_else(|| overflow(purpose.pool))?;
        budget.draw(control_bytes, purpose.pool)?;
        let mut free = FixedVec::new(budget, purpose.ranges, free_slots)?;
        if bytes != 0 {
            free.push(Range { start: 0, bytes })
                .expect("one initial free range");
        }
        let backing = (0..backing_bytes / ALIGNMENT)
            .map(|_| Block {
                _bytes: UnsafeCell::new([MaybeUninit::uninit(); ALIGNMENT]),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let descriptors = (0..leases)
            .map(|_| Descriptor {
                generation: 0,
                range: None,
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let references = (0..leases)
            .map(|_| AtomicUsize::new(0))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Ok(Arc::new(Self {
            purpose,
            backing,
            usable_bytes: bytes,
            references,
            state: Mutex::new(State { descriptors, free }),
        }))
    }

    pub(crate) fn max_entries<T>(&self) -> usize {
        const {
            assert!(align_of::<T>() <= ALIGNMENT);
            assert!(size_of::<T>() != 0);
        }
        self.usable_bytes / size_of::<T>()
    }

    pub(crate) fn lease<T>(
        self: &Arc<Self>,
        entries: usize,
    ) -> Result<MutableSlice<T>, LeaseError> {
        const {
            assert!(align_of::<T>() <= ALIGNMENT);
            assert!(size_of::<T>() != 0);
        }
        let mut lease = MutableSlice {
            pool: Arc::clone(self),
            reservation: None,
            len: 0,
            capacity: 0,
            value: PhantomData,
        };
        lease.reserve_exact(entries)?;
        Ok(lease)
    }

    pub(crate) fn copy_slice<T: Copy>(
        self: &Arc<Self>,
        values: &[T],
    ) -> Result<SharedSlice<T>, LeaseError> {
        let mut lease = self.lease(values.len())?;
        lease.extend_from_slice(values)?;
        Ok(lease.freeze())
    }

    fn pointer<T>(&self, range: Range) -> *mut T {
        let base = self.backing.as_ptr().cast::<u8>();
        // The allocation is stable and aligned; no reference spans other leases.
        unsafe { base.add(range.start).cast_mut().cast::<T>() }
    }

    fn reserve(&self, bytes: usize) -> Result<Reservation, LeaseError> {
        let mut state = self.state.lock().expect("lease pool lock poisoned");
        let requested = rounded(bytes).unwrap_or(usize::MAX);
        let slot = state
            .descriptors
            .iter()
            .position(|d| d.range.is_none() && d.generation < u64::MAX);
        let available = state.free.iter().position(|r| r.bytes >= requested);
        let (Some(slot), Some(index)) = (slot, available) else {
            return Err(state.error(self.purpose, requested));
        };
        let range = Range {
            start: state.free[index].start,
            bytes: requested,
        };
        state.free[index].start += requested;
        state.free[index].bytes -= requested;
        if state.free[index].bytes == 0 {
            state.remove_free(index);
        }
        let descriptor = &mut state.descriptors[slot];
        descriptor.generation += 1;
        descriptor.range = Some(range);
        self.references[slot].store(1, Ordering::Relaxed);
        Ok(Reservation {
            slot,
            generation: descriptor.generation,
            range,
        })
    }

    fn extend(&self, reservation: &mut Reservation, bytes: usize) -> bool {
        let Some(requested) = rounded(bytes) else {
            return false;
        };
        if requested <= reservation.range.bytes {
            return true;
        }
        let mut state = self.state.lock().expect("lease pool lock poisoned");
        state.validate(*reservation);
        let extra = requested - reservation.range.bytes;
        let end = reservation.range.start + reservation.range.bytes;
        let Some(index) = state
            .free
            .iter()
            .position(|r| r.start == end && r.bytes >= extra)
        else {
            return false;
        };
        state.free[index].start += extra;
        state.free[index].bytes -= extra;
        if state.free[index].bytes == 0 {
            state.remove_free(index);
        }
        reservation.range.bytes = requested;
        state.descriptors[reservation.slot].range = Some(reservation.range);
        true
    }

    fn release(&self, reservation: Reservation) {
        let mut state = self.state.lock().expect("lease pool lock poisoned");
        state.validate(reservation);
        debug_assert_eq!(self.references[reservation.slot].load(Ordering::Relaxed), 0);
        state.descriptors[reservation.slot].range = None;
        state.return_range(reservation.range);
    }

    fn release_moved(&self, reservation: Reservation) {
        let prior = self.references[reservation.slot].swap(0, Ordering::Relaxed);
        debug_assert_eq!(prior, 1);
        self.release(reservation);
    }

    fn error(&self, requested: usize) -> LeaseError {
        self.state
            .lock()
            .expect("lease pool lock poisoned")
            .error(self.purpose, requested)
    }
}

impl State {
    fn error(&self, purpose: Purpose, requested_bytes: usize) -> LeaseError {
        LeaseError {
            purpose,
            requested_bytes,
            free_bytes: self.free.iter().map(|r| r.bytes).sum(),
            largest_range_bytes: self.free.iter().map(|r| r.bytes).max().unwrap_or(0),
            available_leases: self
                .descriptors
                .iter()
                .filter(|d| d.range.is_none() && d.generation < u64::MAX)
                .count(),
        }
    }
    fn validate(&self, reservation: Reservation) {
        let descriptor = &self.descriptors[reservation.slot];
        assert_eq!(descriptor.generation, reservation.generation, "stale lease");
        let range = descriptor.range.expect("released lease");
        assert_eq!(
            (range.start, range.bytes),
            (reservation.range.start, reservation.range.bytes)
        );
    }
    fn remove_free(&mut self, index: usize) {
        for at in index + 1..self.free.len() {
            self.free[at - 1] = self.free[at];
        }
        self.free.pop();
    }
    fn return_range(&mut self, range: Range) {
        let index = self
            .free
            .iter()
            .position(|free| free.start > range.start)
            .unwrap_or(self.free.len());
        if index > 0 && self.free[index - 1].start + self.free[index - 1].bytes == range.start {
            self.free[index - 1].bytes += range.bytes;
            if index < self.free.len()
                && self.free[index - 1].start + self.free[index - 1].bytes == self.free[index].start
            {
                self.free[index - 1].bytes += self.free[index].bytes;
                self.remove_free(index);
            }
        } else if index < self.free.len() && range.start + range.bytes == self.free[index].start {
            self.free[index].start = range.start;
            self.free[index].bytes += range.bytes;
        } else {
            self.free
                .push(range)
                .expect("at most live leases plus one free ranges");
            for at in (index + 1..self.free.len()).rev() {
                self.free[at] = self.free[at - 1];
            }
            self.free[index] = range;
        }
    }
}

/// This guard returns the extent even if dropping its initialized values unwinds.
struct Reclaim<'a> {
    pool: &'a LeasePool,
    reservation: Reservation,
}
impl Drop for Reclaim<'_> {
    fn drop(&mut self) {
        self.pool.release(self.reservation);
    }
}

impl<T> MutableSlice<T> {
    pub(crate) fn pool(&self) -> &Arc<LeasePool> {
        &self.pool
    }
    pub(crate) fn len(&self) -> usize {
        self.len
    }
    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }
    pub(crate) fn max_entries(&self) -> usize {
        self.pool.max_entries::<T>()
    }
    pub(crate) fn capacity_entries(&self) -> usize {
        self.capacity
    }

    pub(crate) fn reserve_exact(&mut self, entries: usize) -> Result<(), LeaseError> {
        if entries <= self.capacity {
            return Ok(());
        }
        let bytes = entries
            .checked_mul(size_of::<T>())
            .ok_or_else(|| self.pool.error(usize::MAX))?;
        if let Some(reservation) = &mut self.reservation
            && self.pool.extend(reservation, bytes)
        {
            self.capacity = entries;
            return Ok(());
        }
        // Both extents remain charged until the infallible ownership move finishes.
        let replacement = self.pool.reserve(bytes)?;
        if let Some(prior) = self.reservation {
            // No references into this exclusive owner can coexist with &mut self.
            // The source bytes become uninitialized, so they must not be dropped.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    self.pool.pointer::<T>(prior.range),
                    self.pool.pointer::<T>(replacement.range),
                    self.len,
                );
            }
            self.pool.release_moved(prior);
        }
        self.reservation = Some(replacement);
        self.capacity = entries;
        Ok(())
    }

    pub(crate) fn push(&mut self, value: T) -> Result<(), LeaseError> {
        if self.len == self.capacity {
            let required = self
                .len
                .checked_add(1)
                .ok_or_else(|| self.pool.error(usize::MAX))?;
            let next = self
                .capacity
                .saturating_mul(2)
                .max(1)
                .min(self.max_entries())
                .max(required);
            self.reserve_exact(next)?;
        }
        self.push_reserved(value)
    }

    /// Appends only inside capacity already reserved before publication effects.
    pub(crate) fn push_reserved(&mut self, value: T) -> Result<(), LeaseError> {
        if self.len == self.capacity {
            return Err(self
                .pool
                .error(self.len.saturating_add(1).saturating_mul(size_of::<T>())));
        }
        let reservation = self.reservation.expect("nonempty lease");
        unsafe {
            self.pool
                .pointer::<T>(reservation.range)
                .add(self.len)
                .write(value);
        }
        self.len += 1;
        Ok(())
    }

    pub(crate) fn pop(&mut self) -> Option<T> {
        self.len = self.len.checked_sub(1)?;
        let reservation = self.reservation.expect("initialized lease has backing");
        // Moving the last value out removes it from the prefix dropped by us.
        Some(unsafe {
            self.pool
                .pointer::<T>(reservation.range)
                .add(self.len)
                .read()
        })
    }

    pub(crate) fn extend_from_slice(&mut self, values: &[T]) -> Result<(), LeaseError>
    where
        T: Copy,
    {
        let len = self
            .len
            .checked_add(values.len())
            .ok_or_else(|| self.pool.error(usize::MAX))?;
        self.reserve_exact(len)?;
        if !values.is_empty() {
            let reservation = self.reservation.expect("nonempty lease");
            unsafe {
                core::ptr::copy_nonoverlapping(
                    values.as_ptr(),
                    self.pool.pointer::<T>(reservation.range).add(self.len),
                    values.len(),
                );
            }
        }
        self.len = len;
        Ok(())
    }

    /// Drops the initialized suffix past `len` while keeping the reservation.
    pub(crate) fn truncate(&mut self, len: usize)
    where
        T: Copy,
    {
        self.len = self.len.min(len);
    }

    /// Convert owned elements while retaining the same byte and descriptor
    /// reservation. A panic drops the converted prefix and untouched suffix.
    pub(crate) fn map_in_place<U>(self, mut convert: impl FnMut(T) -> U) -> MutableSlice<U> {
        const {
            assert!(size_of::<T>() != 0);
            assert!(size_of::<T>() == size_of::<U>());
            assert!(align_of::<T>() == align_of::<U>());
        }
        struct Conversion<T, U> {
            source: ManuallyDrop<MutableSlice<T>>,
            converted: usize,
            consumed: usize,
            complete: bool,
            output: PhantomData<U>,
        }
        impl<T, U> Drop for Conversion<T, U> {
            fn drop(&mut self) {
                if self.complete {
                    return;
                }
                let source = &mut *self.source;
                let len = core::mem::take(&mut source.len);
                source.capacity = 0;
                if let Some(reservation) = source.reservation.take() {
                    let prior = source.pool.references[reservation.slot].swap(0, Ordering::Relaxed);
                    debug_assert_eq!(prior, 1);
                    let reclaim = Reclaim {
                        pool: &source.pool,
                        reservation,
                    };
                    // The callback owns the consumed element, including while
                    // unwinding. Only the two disjoint initialized ranges remain.
                    unsafe {
                        core::ptr::drop_in_place(core::ptr::slice_from_raw_parts_mut(
                            source.pool.pointer::<U>(reservation.range),
                            self.converted,
                        ));
                        core::ptr::drop_in_place(core::ptr::slice_from_raw_parts_mut(
                            source
                                .pool
                                .pointer::<T>(reservation.range)
                                .add(self.consumed),
                            len - self.consumed,
                        ));
                    }
                    drop(reclaim);
                }
                // ManuallyDrop suppressed the original owner's destructor.
                unsafe { core::ptr::drop_in_place(&mut source.pool) };
            }
        }
        let mut conversion = Conversion::<T, U> {
            source: ManuallyDrop::new(self),
            converted: 0,
            consumed: 0,
            complete: false,
            output: PhantomData,
        };
        for index in 0..conversion.source.len {
            let reservation = conversion
                .source
                .reservation
                .expect("initialized elements have a reserved range");
            // Each input moves out before its equally aligned output is written.
            let input = unsafe {
                conversion
                    .source
                    .pool
                    .pointer::<T>(reservation.range)
                    .add(index)
                    .read()
            };
            conversion.consumed = index + 1;
            let output = convert(input);
            unsafe {
                conversion
                    .source
                    .pool
                    .pointer::<U>(reservation.range)
                    .add(index)
                    .write(output)
            };
            conversion.converted = index + 1;
        }
        let output = MutableSlice {
            pool: unsafe { core::ptr::read(&conversion.source.pool) },
            reservation: conversion.source.reservation,
            len: conversion.source.len,
            capacity: conversion.source.capacity,
            value: PhantomData,
        };
        conversion.complete = true;
        output
    }

    pub(crate) fn freeze(self) -> SharedSlice<T> {
        if self.len == 0 {
            return SharedSlice::EMPTY;
        }
        let this = ManuallyDrop::new(self);
        // Move the sole pool reference and allocation into the immutable owner.
        SharedSlice {
            pool: Some(unsafe { core::ptr::read(&this.pool) }),
            reservation: this.reservation,
            len: this.len,
            value: PhantomData,
        }
    }

    pub(crate) fn clear(&mut self) {
        let len = core::mem::take(&mut self.len);
        self.capacity = 0;
        if let Some(reservation) = self.reservation.take() {
            let prior = self.pool.references[reservation.slot].swap(0, Ordering::Relaxed);
            debug_assert_eq!(prior, 1);
            let _reclaim = Reclaim {
                pool: &self.pool,
                reservation,
            };
            // Only the initialized prefix owns values. Destructors can use the
            // pool recursively; this range remains occupied until they finish.
            unsafe {
                core::ptr::drop_in_place(core::ptr::slice_from_raw_parts_mut(
                    self.pool.pointer::<T>(reservation.range),
                    len,
                ));
            }
        }
    }

    pub(crate) fn as_slice(&self) -> &[T] {
        let pointer = self
            .reservation
            .map_or(core::ptr::NonNull::<T>::dangling().as_ptr(), |r| {
                self.pool.pointer::<T>(r.range)
            });
        unsafe { core::slice::from_raw_parts(pointer, self.len) }
    }
    pub(crate) fn as_mut_slice(&mut self) -> &mut [T] {
        let pointer = self
            .reservation
            .map_or(core::ptr::NonNull::<T>::dangling().as_ptr(), |r| {
                self.pool.pointer::<T>(r.range)
            });
        unsafe { core::slice::from_raw_parts_mut(pointer, self.len) }
    }
    #[cfg(test)]
    pub(crate) fn slot(&self) -> Option<usize> {
        self.reservation.map(|r| r.slot)
    }
}
impl<T> Deref for MutableSlice<T> {
    type Target = [T];
    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}
impl<T> DerefMut for MutableSlice<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.as_mut_slice()
    }
}
impl<T> AsRef<[T]> for MutableSlice<T> {
    fn as_ref(&self) -> &[T] {
        self.as_slice()
    }
}
impl<T> AsMut<[T]> for MutableSlice<T> {
    fn as_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}
impl<T> Drop for MutableSlice<T> {
    fn drop(&mut self) {
        self.clear();
    }
}

impl<T> SharedSlice<T> {
    pub(crate) const EMPTY: Self = Self {
        pool: None,
        reservation: None,
        len: 0,
        value: PhantomData,
    };
    pub(crate) fn len(&self) -> usize {
        self.len
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub(crate) fn as_slice(&self) -> &[T] {
        let pointer = match (&self.pool, self.reservation) {
            (Some(pool), Some(r)) => pool.pointer::<T>(r.range),
            (None, None) => core::ptr::NonNull::<T>::dangling().as_ptr(),
            _ => unreachable!("shared allocation ownership is complete"),
        };
        unsafe { core::slice::from_raw_parts(pointer, self.len) }
    }
    pub(crate) fn ptr_eq(left: &Self, right: &Self) -> bool {
        match (&left.pool, left.reservation, &right.pool, right.reservation) {
            (None, None, None, None) => true,
            (Some(a), Some(x), Some(b), Some(y)) => {
                Arc::ptr_eq(a, b)
                    && x.slot == y.slot
                    && x.generation == y.generation
                    && left.len == right.len
            }
            _ => false,
        }
    }
}
impl<T> Clone for SharedSlice<T> {
    fn clone(&self) -> Self {
        let Some(pool) = &self.pool else {
            return Self::EMPTY;
        };
        let reservation = self
            .reservation
            .expect("shared allocation has a descriptor");
        let prior = pool.references[reservation.slot].fetch_add(1, Ordering::Relaxed);
        // Match Arc's infallible retain boundary; never wrap into a reusable cell.
        if prior == 0 || prior >= isize::MAX as usize {
            std::process::abort();
        }
        Self {
            pool: Some(Arc::clone(pool)),
            reservation: Some(reservation),
            len: self.len,
            value: PhantomData,
        }
    }
}
impl<T> Deref for SharedSlice<T> {
    type Target = [T];
    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}
impl<T> AsRef<[T]> for SharedSlice<T> {
    fn as_ref(&self) -> &[T] {
        self.as_slice()
    }
}
impl<T: PartialEq> PartialEq for SharedSlice<T> {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}
impl<T: Eq> Eq for SharedSlice<T> {}
impl<T> Drop for SharedSlice<T> {
    fn drop(&mut self) {
        let (Some(pool), Some(reservation)) = (&self.pool, self.reservation) else {
            return;
        };
        if pool.references[reservation.slot].fetch_sub(1, Ordering::Release) != 1 {
            return;
        }
        std::sync::atomic::fence(Ordering::Acquire);
        let _reclaim = Reclaim { pool, reservation };
        unsafe {
            core::ptr::drop_in_place(core::ptr::slice_from_raw_parts_mut(
                pool.pointer::<T>(reservation.range),
                self.len,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(bytes: usize, leases: usize) -> Arc<LeasePool> {
        let mut budget = Budget::new(LeasePool::budget_bytes(bytes, leases));
        let pool = LeasePool::new(&mut budget, Purpose::ROW_WORK, bytes, leases).unwrap();
        assert_eq!(budget.remaining(), 0);
        pool
    }

    #[test]
    fn inplace_conversion_keeps_reserved_slots_and_removes_input_borrows() {
        let pool = pool(128, 4);
        let bytes = Box::new([7u8; 8]);
        crate::mem::guard::forbid_alloc(|| {
            let mut source = pool.lease::<Option<&[u8]>>(4).unwrap();
            source.push_reserved(Some(&bytes[..])).unwrap();
            source.push_reserved(None).unwrap();
            let address = source.as_ptr() as usize;
            let slot = source.slot();
            let mapped: MutableSlice<Option<&'static [u8]>> = source.map_in_place(|_| None);
            drop(bytes);
            assert_eq!(mapped.as_ptr() as usize, address);
            assert_eq!(mapped.slot(), slot);
            assert_eq!((mapped.len(), mapped.capacity()), (2, 4));
            assert!(mapped.iter().all(Option::is_none));
            assert_eq!(pool.lease::<u8>(128).unwrap_err().free_bytes, 64);
            drop(mapped);
            assert_eq!(pool.lease::<u8>(128).unwrap().capacity(), 128);
        });
    }

    #[test]
    fn inplace_conversion_keeps_empty_reserved_capacity_without_running_callback() {
        let pool = pool(128, 2);
        crate::mem::guard::forbid_alloc(|| {
            let source = pool.lease::<u64>(8).unwrap();
            let slot = source.slot();
            let mut called = false;
            let mapped = source.map_in_place(|_| {
                called = true;
                0i64
            });
            assert!(!called);
            assert_eq!(
                (mapped.len(), mapped.capacity(), mapped.slot()),
                (0, 8, slot)
            );
            assert_eq!(pool.lease::<u8>(128).unwrap_err().free_bytes, 64);
            drop(mapped);
            assert_eq!(pool.lease::<u8>(128).unwrap().capacity(), 128);
        });
    }

    #[test]
    fn inplace_conversion_unwinds_both_types_and_allows_recursive_pool_use() {
        struct Before<'a> {
            index: usize,
            count: &'a AtomicUsize,
            pool: Arc<LeasePool>,
        }
        impl Drop for Before<'_> {
            fn drop(&mut self) {
                self.count.fetch_add(1, Ordering::Relaxed);
                drop(self.pool.lease::<u8>(64).unwrap());
            }
        }
        struct After<'a> {
            index: usize,
            count: &'a AtomicUsize,
            pool: Arc<LeasePool>,
        }
        impl Drop for After<'_> {
            fn drop(&mut self) {
                assert!(self.index < 2);
                self.count.fetch_add(1, Ordering::Relaxed);
                drop(self.pool.lease::<u8>(64).unwrap());
            }
        }
        let pool = pool(256, 4);
        let before = AtomicUsize::new(0);
        let after = AtomicUsize::new(0);
        let mut source = pool.lease(4).unwrap();
        for index in 0..4 {
            source
                .push_reserved(Before {
                    index,
                    count: &before,
                    pool: pool.clone(),
                })
                .unwrap();
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            source.map_in_place(|input| {
                assert_ne!(input.index, 2, "stop after two converted elements");
                After {
                    index: input.index,
                    count: &after,
                    pool: input.pool.clone(),
                }
            })
        }));
        assert!(result.is_err());
        assert_eq!(before.load(Ordering::Relaxed), 4);
        assert_eq!(after.load(Ordering::Relaxed), 2);
        assert_eq!(pool.lease::<u8>(256).unwrap().capacity(), 256);
    }

    #[test]
    fn mixed_typed_leases_share_backing_and_release_idle_capacity() {
        let pool = pool(256, 4);
        crate::mem::guard::forbid_alloc(|| {
            let mut ids = pool.lease::<u64>(2).unwrap();
            let mut words = pool.lease::<u128>(2).unwrap();
            ids.push(12).unwrap();
            ids.push(7).unwrap();
            words.push(u128::MAX).unwrap();
            words.push(81).unwrap();
            assert_eq!(words.as_ptr() as usize % align_of::<u128>(), 0);
            ids.as_mut_slice().sort_unstable();
            assert_eq!(ids.as_slice(), &[7, 12]);
            assert_eq!(words.as_slice(), &[u128::MAX, 81]);
            words.clear();
            assert_eq!(words.capacity(), 0);
            words.push(81).unwrap();
            assert_eq!(ids.max_entries(), 32);
            ids.clear();
            assert_eq!((ids.len(), ids.capacity()), (0, 0));
            assert_eq!(words.as_slice(), &[81]);
            drop(words);
            assert!(pool.lease::<u8>(256).is_ok());
        });
    }

    #[test]
    fn fragmented_ranges_coalesce_without_allocating() {
        let pool = pool(256, 4);
        crate::mem::guard::forbid_alloc(|| {
            let one = pool.lease::<u64>(1).unwrap();
            let two = pool.lease::<u64>(1).unwrap();
            let three = pool.lease::<u64>(1).unwrap();
            let four = pool.lease::<u64>(1).unwrap();
            drop(two);
            drop(four);
            let error = pool.lease::<u8>(128).unwrap_err();
            assert_eq!(error.requested_bytes, 128);
            assert_eq!(error.free_bytes, 128);
            assert_eq!(error.largest_range_bytes, 64);
            assert_eq!(error.available_leases, 2);
            drop(three);
            let joined = pool.lease::<u8>(192).unwrap();
            drop(one);
            drop(joined);
            assert!(pool.lease::<u8>(256).is_ok());
        });
    }

    #[test]
    fn lease_growth_charges_old_and_new_ranges_and_preserves_failure() {
        let pool = pool(256, 4);
        crate::mem::guard::forbid_alloc(|| {
            let mut ids = pool.lease::<u64>(1).unwrap();
            ids.push(42).unwrap();
            let blocker = pool.lease::<u8>(64).unwrap();
            let original = ids.slot();
            ids.reserve_exact(16).unwrap();
            assert_ne!(ids.slot(), original);
            assert_eq!(ids.as_slice(), &[42]);
            assert_eq!(ids.capacity(), 16);
            let error = ids.reserve_exact(32).unwrap_err();
            assert_eq!(error.free_bytes, 64);
            assert_eq!(ids.as_slice(), &[42]);
            assert_eq!(ids.capacity(), 16);
            drop(blocker);
            ids.clear();
            ids.reserve_exact(32).unwrap();
            assert_eq!(ids.capacity(), 32);
        });
    }

    #[test]
    fn growth_in_place_needs_no_extra_descriptor() {
        let pool = pool(128, 1);
        crate::mem::guard::forbid_alloc(|| {
            let mut values = pool.lease::<u64>(1).unwrap();
            values.push(13).unwrap();
            let slot = values.slot();
            for value in 1..16 {
                values.push(value).unwrap();
            }
            assert_eq!(values.slot(), slot);
            assert_eq!(values.len(), 16);
            assert_eq!(values[0], 13);
            assert_eq!(values[15], 15);
            let error = pool.lease::<u8>(1).unwrap_err();
            assert_eq!(error.available_leases, 0);
        });
    }

    #[test]
    fn lease_generation_never_wraps_and_empty_handles_cost_no_descriptor() {
        let pool = pool(64, 1);
        pool.state.lock().unwrap().descriptors[0].generation = u64::MAX - 1;
        crate::mem::guard::forbid_alloc(|| {
            let mut last = pool.lease::<u64>(1).unwrap();
            last.push(9).unwrap();
            assert_eq!(last.reservation.unwrap().generation, u64::MAX);
            drop(last);
            let error = pool.lease::<u8>(1).unwrap_err();
            assert_eq!((error.free_bytes, error.available_leases), (64, 0));
            let empty = pool.lease::<u128>(0).unwrap();
            assert!(empty.is_empty());
            assert_eq!(empty.capacity(), 0);
        });
    }

    #[test]
    fn owned_typed_leases_survive_owner_moves_and_release_during_unwind() {
        let pool = pool(128, 2);
        let mut retained = pool.lease::<u64>(2).unwrap();
        retained.push(27).unwrap();
        retained.push(31).unwrap();
        let other = Arc::clone(&pool);
        drop(pool);
        crate::mem::guard::forbid_alloc(|| {
            let moved = retained;
            assert_eq!(moved.as_slice(), &[27, 31]);
            let mut nested = other.lease::<u128>(1).unwrap();
            nested.push(u128::MAX).unwrap();
            assert_eq!(moved.as_slice(), &[27, 31]);
            drop(moved);
            drop(nested);
            assert!(other.lease::<u8>(128).is_ok());
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut lease = other.lease::<u64>(16).unwrap();
            lease.push(1).unwrap();
            panic!("release a partially initialized lease");
        }));
        assert!(result.is_err());
        assert!(other.lease::<u8>(128).is_ok());
    }

    #[test]
    fn parallel_leases_keep_payloads_disjoint_without_callback_locks() {
        let pool = pool(128, 2);
        let together = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let worker = |value| {
                crate::mem::guard::forbid_alloc(|| {
                    let mut lease = pool.lease::<u64>(8).unwrap();
                    for _ in 0..8 {
                        lease.push(value).unwrap();
                    }
                    together.wait();
                    for entry in lease.iter() {
                        assert_eq!(*entry, value);
                    }
                    together.wait();
                });
            };
            let left = scope.spawn(move || worker(71));
            let right = scope.spawn(move || worker(93));
            left.join().unwrap();
            right.join().unwrap();
        });
        assert!(pool.lease::<u8>(128).is_ok());
    }

    #[test]
    fn startup_rejects_misaligned_capacity_before_reserving_any_bytes() {
        let mut budget = Budget::new(1 << 20);
        assert_eq!(
            LeasePool::new(&mut budget, Purpose::ROW_WORK, 65, 2)
                .err()
                .unwrap(),
            LeaseSetupError::Alignment {
                setting: "row_work_bytes",
                bytes: 65
            }
        );
        assert_eq!(budget.used(), 0);
        let zero_bytes = LeasePool::budget_bytes(0, 0);
        let mut zero_budget = Budget::new(zero_bytes);
        let empty = LeasePool::new(&mut zero_budget, Purpose::ROW_WORK, 0, 0).unwrap();
        assert_eq!(zero_budget.remaining(), 0);
        assert_eq!(empty.max_entries::<u64>(), 0);
        assert!(empty.lease::<u64>(0).is_ok());
        let mut short = Budget::new(0);
        assert!(matches!(
            LeasePool::new(&mut short, Purpose::ROW_WORK, 64, 1),
            Err(LeaseSetupError::Budget(_))
        ));
        assert_eq!(short.used(), 0);
    }

    #[test]
    fn startup_overflow_and_typed_length_overflow_are_named_errors() {
        let mut budget = Budget::new(1024);
        let too_many_bytes = usize::MAX & !(ALIGNMENT - 1);
        assert!(matches!(
            LeasePool::new(&mut budget, Purpose::ROW_WORK, too_many_bytes, 1),
            Err(LeaseSetupError::CapacityOverflow { .. })
        ));
        assert_eq!(budget.used(), 0);
        assert!(matches!(
            LeasePool::new(&mut budget, Purpose::ROW_WORK, 64, usize::MAX),
            Err(LeaseSetupError::CapacityOverflow { .. })
        ));
        assert_eq!(budget.used(), 0);
        let too_many_descriptors = isize::MAX as usize / size_of::<Descriptor>() + 1;
        assert!(matches!(
            LeasePool::new(&mut budget, Purpose::ROW_WORK, 64, too_many_descriptors),
            Err(LeaseSetupError::CapacityOverflow { .. })
        ));
        assert_eq!(budget.used(), 0);
        let large_backing = ((isize::MAX as usize / 4) * 3) & !(ALIGNMENT - 1);
        let large_descriptors = isize::MAX as usize / size_of::<Descriptor>();
        assert!(matches!(
            LeasePool::new(
                &mut budget,
                Purpose::ROW_WORK,
                large_backing,
                large_descriptors
            ),
            Err(LeaseSetupError::CapacityOverflow { .. })
        ));
        assert_eq!(budget.used(), 0);
        let pool = pool(64, 1);
        crate::mem::guard::forbid_alloc(|| {
            let error = pool.lease::<u64>(usize::MAX).unwrap_err();
            assert_eq!(error.requested_bytes, usize::MAX);
            assert_eq!(error.free_bytes, 64);
            assert!(pool.lease::<u64>(8).is_ok());
        });
    }

    struct Counted<'a> {
        value: u64,
        drops: &'a AtomicUsize,
    }
    impl Drop for Counted<'_> {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn noncopy_growth_moves_initialized_values_and_failed_push_drops_once() {
        let pool = pool(256, 4);
        let drops = AtomicUsize::new(0);
        crate::mem::guard::forbid_alloc(|| {
            let mut values = pool.lease::<Counted<'_>>(1).unwrap();
            values
                .push_reserved(Counted {
                    value: 17,
                    drops: &drops,
                })
                .unwrap();
            let failed = values.push_reserved(Counted {
                value: 99,
                drops: &drops,
            });
            assert!(failed.is_err());
            assert_eq!(drops.load(Ordering::Relaxed), 1);
            let blocker = pool.lease::<u8>(64).unwrap();
            let original = values.slot();
            values.reserve_exact(8).unwrap();
            assert_ne!(values.slot(), original);
            assert_eq!(values[0].value, 17);
            assert_eq!(drops.load(Ordering::Relaxed), 1);
            assert!(values.reserve_exact(32).is_err());
            values
                .push_reserved(Counted {
                    value: 23,
                    drops: &drops,
                })
                .unwrap();
            assert_eq!(values.len(), 2);
            assert_eq!(values[1].value, 23);
            drop(values);
            assert_eq!(drops.load(Ordering::Relaxed), 3);
            drop(blocker);
            assert!(pool.lease::<u8>(256).is_ok());
        });
    }

    #[test]
    fn noncopy_pop_moves_one_value_without_drop_or_capacity_change() {
        let pool = pool(64, 1);
        let drops = AtomicUsize::new(0);
        crate::mem::guard::forbid_alloc(|| {
            let mut values = pool.lease::<Counted<'_>>(2).unwrap();
            values
                .push_reserved(Counted {
                    value: 31,
                    drops: &drops,
                })
                .unwrap();
            values
                .push_reserved(Counted {
                    value: 37,
                    drops: &drops,
                })
                .unwrap();
            let popped = values.pop().unwrap();
            assert_eq!(popped.value, 37);
            assert_eq!((values.len(), values.capacity()), (1, 2));
            assert_eq!(drops.load(Ordering::Relaxed), 0);
            drop(values);
            assert_eq!(drops.load(Ordering::Relaxed), 1);
            drop(popped);
            assert_eq!(drops.load(Ordering::Relaxed), 2);
            let mut empty = pool.lease::<Counted<'_>>(0).unwrap();
            assert!(empty.pop().is_none());
            assert!(pool.lease::<u8>(64).is_ok());
        });
    }

    #[test]
    fn frozen_noncopy_pins_share_one_allocation_until_last_owner() {
        let pool = pool(128, 2);
        let drops = AtomicUsize::new(0);
        crate::mem::guard::forbid_alloc(|| {
            let mut values = pool.lease::<Counted<'_>>(2).unwrap();
            values
                .push_reserved(Counted {
                    value: 41,
                    drops: &drops,
                })
                .unwrap();
            values
                .push_reserved(Counted {
                    value: 43,
                    drops: &drops,
                })
                .unwrap();
            let frozen = values.freeze();
            let blocker = pool.lease::<u8>(64).unwrap();
            assert!(pool.lease::<u8>(1).is_err());
            let retained = frozen.clone();
            assert!(SharedSlice::ptr_eq(&frozen, &retained));
            drop(frozen);
            assert_eq!(retained[0].value, 41);
            assert_eq!(retained[1].value, 43);
            assert_eq!(drops.load(Ordering::Relaxed), 0);
            drop(retained);
            assert_eq!(drops.load(Ordering::Relaxed), 2);
            let reused = pool.lease::<u8>(64).unwrap();
            drop((reused, blocker));
            assert!(pool.lease::<u8>(128).is_ok());
        });
    }

    #[test]
    fn frozen_slices_survive_their_allocator_owner_and_empty_slices_are_free() {
        let empty_pool = pool(64, 1);
        let pool = pool(64, 1);
        let drops = AtomicUsize::new(0);
        let mut values = pool.lease::<Counted<'_>>(1).unwrap();
        values
            .push_reserved(Counted {
                value: 73,
                drops: &drops,
            })
            .unwrap();
        let frozen = values.freeze();
        drop(pool);
        crate::mem::guard::forbid_alloc(|| {
            let retained = frozen.clone();
            let empty = SharedSlice::<Counted<'_>>::EMPTY;
            let empty_clone = empty.clone();
            assert!(empty_clone.is_empty());
            assert!(SharedSlice::ptr_eq(&empty, &empty_clone));
            drop(frozen);
            assert_eq!(retained[0].value, 73);
            drop(retained);
            assert_eq!(drops.load(Ordering::Relaxed), 1);
            let reserved_empty = empty_pool.lease::<u8>(64).unwrap().freeze();
            assert!(reserved_empty.is_empty());
            assert!(empty_pool.lease::<u8>(64).is_ok());
        });
    }

    #[test]
    fn final_shared_drop_is_reentrant_and_does_not_reuse_retiring_extent() {
        struct Reentrant<'a> {
            pool: Arc<LeasePool>,
            drops: &'a AtomicUsize,
        }
        impl Drop for Reentrant<'_> {
            fn drop(&mut self) {
                assert!(self.pool.lease::<u8>(128).is_err());
                let mut other = self.pool.lease::<u8>(64).unwrap();
                other.push_reserved(5).unwrap();
                assert_eq!(other[0], 5);
                self.drops.fetch_add(1, Ordering::Relaxed);
            }
        }
        let pool = pool(128, 2);
        let drops = AtomicUsize::new(0);
        crate::mem::guard::forbid_alloc(|| {
            let mut values = pool.lease::<Reentrant<'_>>(1).unwrap();
            values
                .push_reserved(Reentrant {
                    pool: Arc::clone(&pool),
                    drops: &drops,
                })
                .unwrap();
            let frozen = values.freeze();
            let retained = frozen.clone();
            drop(frozen);
            assert_eq!(drops.load(Ordering::Relaxed), 0);
            drop(retained);
            assert_eq!(drops.load(Ordering::Relaxed), 1);
            assert!(pool.lease::<u8>(128).is_ok());
        });
    }

    #[test]
    fn unwinding_noncopy_destructors_drop_remaining_prefix_then_reclaim() {
        struct PanicOnce<'a> {
            value: usize,
            drops: &'a AtomicUsize,
        }
        impl Drop for PanicOnce<'_> {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::Relaxed);
                if self.value == 1 {
                    panic!("unwind the retained initialized prefix");
                }
            }
        }
        let pool = pool(128, 1);
        let drops = AtomicUsize::new(0);
        let mut values = pool.lease::<PanicOnce<'_>>(4).unwrap();
        for value in 0..3 {
            values
                .push_reserved(PanicOnce {
                    value,
                    drops: &drops,
                })
                .unwrap();
        }
        let retained = values.freeze();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(retained)));
        assert!(result.is_err());
        assert_eq!(drops.load(Ordering::Relaxed), 3);
        assert!(pool.lease::<u8>(128).is_ok());
    }

    #[test]
    fn parallel_shared_readers_keep_noncopy_values_pinned() {
        let pool = pool(128, 2);
        let drops = AtomicUsize::new(0);
        let mut values = pool.lease::<Counted<'_>>(1).unwrap();
        values
            .push_reserved(Counted {
                value: 83,
                drops: &drops,
            })
            .unwrap();
        let retained = values.freeze();
        let together = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let shared = retained.clone();
            let worker = scope.spawn(|| {
                crate::mem::guard::forbid_alloc(|| {
                    together.wait();
                    assert_eq!(shared[0].value, 83);
                    together.wait();
                    drop(shared);
                });
            });
            crate::mem::guard::forbid_alloc(|| {
                together.wait();
                let mut other = pool.lease::<u64>(1).unwrap();
                other.push_reserved(97).unwrap();
                assert_eq!(retained[0].value, 83);
                drop(retained);
                assert_eq!(drops.load(Ordering::Relaxed), 0);
                together.wait();
            });
            worker.join().unwrap();
        });
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert!(pool.lease::<u8>(128).is_ok());
    }

    #[test]
    fn purpose_labels_name_metadata_limits_and_shared_generation_never_wraps() {
        let mut budget = Budget::new(LeasePool::budget_bytes(64, 1));
        assert_eq!(
            LeasePool::new(&mut budget, Purpose::TABLE_METADATA, 65, 1)
                .err()
                .unwrap(),
            LeaseSetupError::Alignment {
                setting: "table_metadata_bytes",
                bytes: 65
            }
        );
        assert_eq!(budget.used(), 0);
        let pool = LeasePool::new(&mut budget, Purpose::TABLE_METADATA, 64, 1).unwrap();
        pool.state.lock().unwrap().descriptors[0].generation = u64::MAX - 1;
        crate::mem::guard::forbid_alloc(|| {
            let mut values = pool.lease::<u64>(1).unwrap();
            values.push_reserved(101).unwrap();
            let retained = values.freeze();
            let shared = retained.clone();
            drop(retained);
            assert_eq!(shared[0], 101);
            let error = pool.lease::<u64>(1).unwrap_err();
            assert_eq!(error.purpose, Purpose::TABLE_METADATA);
            assert_eq!((error.free_bytes, error.available_leases), (0, 0));
            drop(shared);
            let error = pool.lease::<u64>(1).unwrap_err();
            assert_eq!((error.free_bytes, error.available_leases), (64, 0));
        });
    }
}
