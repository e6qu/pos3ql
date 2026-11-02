//! Typed, individually released ranges in one startup-allocated byte pool.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::{MaybeUninit, align_of, size_of};
use core::ops::{Deref, DerefMut};
use {crate::mem::mutex::Mutex, std::sync::Arc};

use super::budget::{Budget, BudgetError};
use super::fixed_vec::FixedVec;

const ALIGNMENT: usize = 64;

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

/// The backing and all allocator metadata are allocated at startup. Ranges
/// start on 64-byte boundaries; each live lease wastes at most 63 padding bytes.
pub(crate) struct RowWorkPool {
    backing: Box<[Block]>,
    usable_bytes: usize,
    state: Mutex<State>,
}

// A live descriptor exclusively owns a disjoint range. Only its typed lease
// accesses those bytes, and that lease inherits T's Send/Sync requirements.
// The allocator mutex protects descriptors, never payload access or callbacks.
unsafe impl Send for RowWorkPool {}
unsafe impl Sync for RowWorkPool {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LeaseError {
    pub requested_bytes: usize,
    pub free_bytes: usize,
    pub largest_range_bytes: usize,
    pub available_leases: usize,
}

impl core::fmt::Display for LeaseError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            formatter,
            "row work pool exhausted: requested {} bytes, {} free bytes, largest range {} bytes, {} available leases",
            self.requested_bytes, self.free_bytes, self.largest_range_bytes, self.available_leases
        )
    }
}

impl std::error::Error for LeaseError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RowWorkSetupError {
    Budget(BudgetError),
    Alignment { bytes: usize },
    CapacityOverflow { what: &'static str },
}

impl From<BudgetError> for RowWorkSetupError {
    fn from(error: BudgetError) -> Self {
        Self::Budget(error)
    }
}

impl core::fmt::Display for RowWorkSetupError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Budget(error) => core::fmt::Display::fmt(error, formatter),
            Self::Alignment { bytes } => write!(
                formatter,
                "row_work_bytes must be a multiple of 64 bytes (got {bytes})"
            ),
            Self::CapacityOverflow { what } => write!(
                formatter,
                "{what} exceeds the supported row work memory layout"
            ),
        }
    }
}

impl std::error::Error for RowWorkSetupError {}

#[derive(Clone, Copy)]
struct Reservation {
    slot: usize,
    generation: u64,
    range: Range,
}

/// Only the initialized prefix is exposed. Moving or dropping a lease does
/// not depend on the lifetime or address of the caller that acquired it.
pub(crate) struct RowWorkLease<T: Copy> {
    pool: Arc<RowWorkPool>,
    reservation: Option<Reservation>,
    len: usize,
    capacity: usize,
    value: PhantomData<T>,
}

impl<T: Copy> core::fmt::Debug for RowWorkLease<T> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("RowWorkLease")
            .field("len", &self.len)
            .field("capacity", &self.capacity)
            .finish_non_exhaustive()
    }
}

fn rounded(bytes: usize) -> Option<usize> {
    bytes
        .checked_add(ALIGNMENT - 1)
        .map(|n| n & !(ALIGNMENT - 1))
}

impl RowWorkPool {
    pub(crate) fn budget_bytes(bytes: usize, leases: usize) -> usize {
        size_of::<Self>()
            .saturating_add(2 * size_of::<usize>())
            .saturating_add(rounded(bytes).unwrap_or(usize::MAX))
            .saturating_add(leases.saturating_mul(size_of::<Descriptor>()))
            .saturating_add(leases.saturating_add(1).saturating_mul(size_of::<Range>()))
    }

    pub(crate) fn new(
        budget: &mut Budget,
        bytes: usize,
        leases: usize,
    ) -> Result<Arc<Self>, RowWorkSetupError> {
        if !bytes.is_multiple_of(ALIGNMENT) {
            return Err(RowWorkSetupError::Alignment { bytes });
        }
        let overflow = |what| RowWorkSetupError::CapacityOverflow { what };
        let backing_bytes = rounded(bytes).ok_or_else(|| overflow("row_work_bytes"))?;
        let free_slots = leases
            .checked_add(1)
            .ok_or_else(|| overflow("row_work_leases"))?;
        std::alloc::Layout::array::<Block>(backing_bytes / ALIGNMENT)
            .map_err(|_| overflow("row work backing"))?;
        std::alloc::Layout::array::<Descriptor>(leases)
            .map_err(|_| overflow("row work lease descriptors"))?;
        std::alloc::Layout::array::<Range>(free_slots)
            .map_err(|_| overflow("row work free ranges"))?;
        let descriptors_bytes = leases
            .checked_mul(size_of::<Descriptor>())
            .ok_or_else(|| overflow("row work lease descriptors"))?;
        let control_bytes = (size_of::<Self>() + 2 * size_of::<usize>())
            .checked_add(backing_bytes)
            .and_then(|n| n.checked_add(descriptors_bytes))
            .ok_or_else(|| overflow("row work startup controls"))?;
        let free_bytes = free_slots
            .checked_mul(size_of::<Range>())
            .ok_or_else(|| overflow("row work free ranges"))?;
        control_bytes
            .checked_add(free_bytes)
            .ok_or_else(|| overflow("row work startup allocation"))?;
        budget.draw(control_bytes, "row work pool")?;
        let mut free = FixedVec::new(budget, "row work free ranges", free_slots)?;
        let usable_bytes = bytes;
        if usable_bytes != 0 {
            free.push(Range {
                start: 0,
                bytes: usable_bytes,
            })
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
        Ok(Arc::new(Self {
            backing,
            usable_bytes,
            state: Mutex::new(State { descriptors, free }),
        }))
    }

    pub(crate) fn max_entries<T: Copy>(&self) -> usize {
        const {
            assert!(align_of::<T>() <= ALIGNMENT);
            assert!(size_of::<T>() != 0);
        }
        self.usable_bytes / size_of::<T>()
    }

    pub(crate) fn lease<T: Copy>(
        self: &Arc<Self>,
        entries: usize,
    ) -> Result<RowWorkLease<T>, LeaseError> {
        const {
            assert!(align_of::<T>() <= ALIGNMENT);
            assert!(size_of::<T>() != 0);
        }
        let mut lease = RowWorkLease {
            pool: Arc::clone(self),
            reservation: None,
            len: 0,
            capacity: 0,
            value: PhantomData,
        };
        lease.reserve_exact(entries)?;
        Ok(lease)
    }

    fn pointer<T>(&self, range: Range) -> *mut T {
        // Box's allocation is stable and 64-byte aligned. Address computation
        // crosses blocks without deriving a reference covering another lease.
        let base = self.backing.as_ptr().cast::<u8>();
        unsafe { base.add(range.start).cast_mut().cast::<T>() }
    }

    fn reserve(&self, bytes: usize) -> Result<Reservation, LeaseError> {
        let mut state = self.state.lock().expect("row work pool lock poisoned");
        let requested = rounded(bytes).unwrap_or(usize::MAX);
        let slot = state
            .descriptors
            .iter()
            .position(|d| d.range.is_none() && d.generation < u64::MAX);
        let available = state.free.iter().position(|range| range.bytes >= requested);
        let (Some(slot), Some(index)) = (slot, available) else {
            return Err(state.error(requested));
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
        let mut state = self.state.lock().expect("row work pool lock poisoned");
        state.validate(*reservation);
        let extra = requested - reservation.range.bytes;
        let end = reservation.range.start + reservation.range.bytes;
        let Some(index) = state
            .free
            .iter()
            .position(|range| range.start == end && range.bytes >= extra)
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
        let mut state = self.state.lock().expect("row work pool lock poisoned");
        state.validate(reservation);
        state.descriptors[reservation.slot].range = None;
        state.return_range(reservation.range);
    }

    fn error(&self, requested: usize) -> LeaseError {
        self.state
            .lock()
            .expect("row work pool lock poisoned")
            .error(requested)
    }
}

impl State {
    fn error(&self, requested_bytes: usize) -> LeaseError {
        LeaseError {
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
        assert_eq!(
            descriptor.generation, reservation.generation,
            "stale row work lease"
        );
        let range = descriptor.range.expect("released row work lease");
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

impl<T: Copy> RowWorkLease<T> {
    pub(crate) fn pool(&self) -> &Arc<RowWorkPool> {
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
        // Both ranges and both descriptors are charged until the copy is
        // complete. Failure leaves the previous initialized prefix untouched.
        let replacement = self.pool.reserve(bytes)?;
        if let Some(prior) = self.reservation {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    self.pool.pointer::<T>(prior.range),
                    self.pool.pointer::<T>(replacement.range),
                    self.len,
                );
            }
            self.pool.release(prior);
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
        let reservation = self.reservation.expect("nonempty row work lease");
        unsafe {
            self.pool
                .pointer::<T>(reservation.range)
                .add(self.len)
                .write(value);
        }
        self.len += 1;
        Ok(())
    }

    /// Returning idle capacity lets another reader or writer use these bytes.
    pub(crate) fn clear(&mut self) {
        if let Some(reservation) = self.reservation.take() {
            self.pool.release(reservation);
        }
        self.len = 0;
        self.capacity = 0;
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

impl<T: Copy> Deref for RowWorkLease<T> {
    type Target = [T];
    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

impl<T: Copy> DerefMut for RowWorkLease<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.as_mut_slice()
    }
}

impl<T: Copy> Drop for RowWorkLease<T> {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(bytes: usize, leases: usize) -> Arc<RowWorkPool> {
        let mut budget = Budget::new(RowWorkPool::budget_bytes(bytes, leases));
        let pool = RowWorkPool::new(&mut budget, bytes, leases).unwrap();
        assert_eq!(budget.remaining(), 0);
        pool
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
            RowWorkPool::new(&mut budget, 65, 2).err().unwrap(),
            RowWorkSetupError::Alignment { bytes: 65 }
        );
        assert_eq!(budget.used(), 0);
        let zero_bytes = RowWorkPool::budget_bytes(0, 0);
        let mut zero_budget = Budget::new(zero_bytes);
        let empty = RowWorkPool::new(&mut zero_budget, 0, 0).unwrap();
        assert_eq!(zero_budget.remaining(), 0);
        assert_eq!(empty.max_entries::<u64>(), 0);
        assert!(empty.lease::<u64>(0).is_ok());
        let mut short = Budget::new(0);
        assert!(matches!(
            RowWorkPool::new(&mut short, 64, 1),
            Err(RowWorkSetupError::Budget(_))
        ));
        assert_eq!(short.used(), 0);
    }

    #[test]
    fn startup_overflow_and_typed_length_overflow_are_named_errors() {
        let mut budget = Budget::new(1024);
        let too_many_bytes = usize::MAX & !(ALIGNMENT - 1);
        assert!(matches!(
            RowWorkPool::new(&mut budget, too_many_bytes, 1),
            Err(RowWorkSetupError::CapacityOverflow { .. })
        ));
        assert_eq!(budget.used(), 0);
        assert!(matches!(
            RowWorkPool::new(&mut budget, 64, usize::MAX),
            Err(RowWorkSetupError::CapacityOverflow { .. })
        ));
        assert_eq!(budget.used(), 0);
        let too_many_descriptors = isize::MAX as usize / size_of::<Descriptor>() + 1;
        assert!(matches!(
            RowWorkPool::new(&mut budget, 64, too_many_descriptors),
            Err(RowWorkSetupError::CapacityOverflow { .. })
        ));
        assert_eq!(budget.used(), 0);
        let large_backing = ((isize::MAX as usize / 4) * 3) & !(ALIGNMENT - 1);
        let large_descriptors = isize::MAX as usize / size_of::<Descriptor>();
        assert!(matches!(
            RowWorkPool::new(&mut budget, large_backing, large_descriptors),
            Err(RowWorkSetupError::CapacityOverflow { .. })
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
}
