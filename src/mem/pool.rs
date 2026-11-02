//! A fixed-size object pool with generational handles.
//!
//! Handles are indices tagged with a generation; using a handle after its
//! slot was released panics with the pool's name rather than silently
//! reading another object's data (the TigerBeetle-style answer to
//! use-after-free in index-based designs).

use core::fmt;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicUsize, Ordering};

static NEXT_POOL_ID: AtomicUsize = AtomicUsize::new(1);

use super::budget::{Budget, BudgetError};
use super::fixed_vec::FixedVec;

pub struct Pool<T> {
    what: &'static str,
    identity: usize,
    values: Box<[MaybeUninit<T>]>,
    generations: Box<[u32]>,
    occupied: Box<[bool]>,
    free: FixedVec<u32>,
    live: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Handle {
    pool: usize,
    index: u32,
    generation: u32,
}

impl Handle {
    /// The raw slot index, for per-slot companion arrays owned by the caller.
    pub fn index(self) -> usize {
        self.index as usize
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolExhausted {
    pub what: &'static str,
    pub capacity: usize,
}

impl fmt::Display for PoolExhausted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "pool '{}' exhausted (capacity {})",
            self.what, self.capacity
        )
    }
}

impl std::error::Error for PoolExhausted {}

fn allocate_pool_identity(counter: &AtomicUsize, budget: &Budget) -> Result<usize, BudgetError> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .map_err(|_| BudgetError {
            what: "pool handle identities",
            requested: usize::MAX,
            remaining: budget.remaining(),
            total: budget.total(),
        })
}

impl<T> Pool<T> {
    pub fn new(
        budget: &mut Budget,
        what: &'static str,
        capacity: usize,
    ) -> Result<Self, BudgetError> {
        assert!(
            capacity <= u32::MAX as usize,
            "pool '{what}' capacity overflows u32"
        );
        let identity = allocate_pool_identity(&NEXT_POOL_ID, budget)?;
        budget.draw_array(capacity, size_of::<T>() + size_of::<u32>() + 1, what)?;
        let mut free = FixedVec::new(budget, what, capacity)?;
        for index in (0..capacity as u32).rev() {
            free.push(index).expect("free list sized to capacity");
        }
        Ok(Self {
            what,
            identity,
            values: Box::new_uninit_slice(capacity),
            generations: vec![0; capacity].into_boxed_slice(),
            occupied: vec![false; capacity].into_boxed_slice(),
            free,
            live: 0,
        })
    }

    pub fn acquire(&mut self, value: T) -> Result<Handle, PoolExhausted> {
        let Some(index) = self.free.pop() else {
            return Err(PoolExhausted {
                what: self.what,
                capacity: self.values.len(),
            });
        };
        let i = index as usize;
        self.values[i].write(value);
        self.occupied[i] = true;
        self.live += 1;
        Ok(Handle {
            pool: self.identity,
            index,
            generation: self.generations[i],
        })
    }

    pub fn release(&mut self, handle: Handle) -> T {
        let i = self.check(handle);
        self.occupied[i] = false;
        self.live -= 1;
        // Retire exhausted generations permanently: wrapping could revive an
        // old handle after enough reuse. Retired cells are neither free nor live.
        if let Some(generation) = self.generations[i].checked_add(1) {
            self.generations[i] = generation;
            self.free
                .push(handle.index)
                .expect("free list sized to capacity");
        }
        unsafe { self.values[i].assume_init_read() }
    }

    pub fn get(&self, handle: Handle) -> &T {
        let i = self.check(handle);
        unsafe { self.values[i].assume_init_ref() }
    }

    pub fn get_mut(&mut self, handle: Handle) -> &mut T {
        let i = self.check(handle);
        unsafe { self.values[i].assume_init_mut() }
    }

    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn capacity(&self) -> usize {
        self.values.len()
    }

    /// Live handles in unspecified order.
    fn check(&self, handle: Handle) -> usize {
        let i = handle.index as usize;
        assert!(
            handle.pool == self.identity
                && i < self.values.len()
                && self.occupied[i]
                && self.generations[i] == handle.generation,
            "pool '{}': stale or invalid handle {:?}",
            self.what,
            handle
        );
        i
    }
}

impl<T> Drop for Pool<T> {
    fn drop(&mut self) {
        for i in 0..self.values.len() {
            if self.occupied[i] {
                unsafe { self.values[i].assume_init_drop() };
            }
        }
    }
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::*;

    #[test]
    fn acquire_get_release_roundtrip() {
        let mut budget = Budget::new(4096);
        let mut pool: Pool<String> = Pool::new(&mut budget, "test", 4).unwrap();
        let h = pool.acquire("hello".to_string()).unwrap();
        assert_eq!(pool.get(h), "hello");
        pool.get_mut(h).push_str(" world");
        assert_eq!(pool.release(h), "hello world");
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn exhaustion_is_a_named_error() {
        let mut budget = Budget::new(4096);
        let mut pool: Pool<u64> = Pool::new(&mut budget, "connections", 2).unwrap();
        pool.acquire(1).unwrap();
        pool.acquire(2).unwrap();
        let err = pool.acquire(3).unwrap_err();
        assert_eq!(err.what, "connections");
        assert_eq!(err.capacity, 2);
    }

    #[test]
    #[should_panic(expected = "stale or invalid handle")]
    fn stale_handle_panics() {
        let mut budget = Budget::new(4096);
        let mut pool: Pool<u64> = Pool::new(&mut budget, "test", 2).unwrap();
        let h = pool.acquire(1).unwrap();
        pool.release(h);
        // The slot is reused with a new generation; the old handle must die.
        let _h2 = pool.acquire(2).unwrap();
        pool.get(h);
    }

    #[test]
    fn released_slots_are_reused() {
        let mut budget = Budget::new(4096);
        let mut pool: Pool<u64> = Pool::new(&mut budget, "test", 2).unwrap();
        for round in 0..100 {
            let a = pool.acquire(round).unwrap();
            let b = pool.acquire(round + 1).unwrap();
            assert!(pool.acquire(0).is_err());
            assert_eq!(pool.release(a), round);
            assert_eq!(pool.release(b), round + 1);
        }
    }

    #[test]
    fn drop_runs_destructors_of_live_values() {
        use std::rc::Rc;
        let marker = Rc::new(());
        let mut budget = Budget::new(4096);
        {
            let mut pool = Pool::new(&mut budget, "test", 4).unwrap();
            let _a = pool.acquire(Rc::clone(&marker)).unwrap();
            let b = pool.acquire(Rc::clone(&marker)).unwrap();
            pool.release(b);
            assert_eq!(Rc::strong_count(&marker), 2);
        }
        assert_eq!(Rc::strong_count(&marker), 1);
    }

    #[test]
    fn hot_path_does_not_allocate() {
        let mut budget = Budget::new(4096);
        let mut pool: Pool<u64> = Pool::new(&mut budget, "test", 8).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let handles: [Handle; 8] = core::array::from_fn(|i| pool.acquire(i as u64).unwrap());
            for h in handles {
                pool.release(h);
            }
        });
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;

    #[test]
    fn generation_exhaustion_retires_cells_and_counts_only_live_values() {
        use core::sync::atomic::{AtomicUsize, Ordering};
        struct Counted<'a>(&'a AtomicUsize);
        impl Drop for Counted<'_> {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let dropped = AtomicUsize::new(0);
        let mut budget = Budget::new(4096);
        let mut pool = Pool::new(&mut budget, "retiring handles", 2).unwrap();
        pool.generations[0] = u32::MAX - 1;
        crate::mem::guard::forbid_alloc(|| {
            let prior = pool.acquire(Counted(&dropped)).unwrap();
            assert_eq!(prior.generation, u32::MAX - 1);
            drop(pool.release(prior));
            let last = pool.acquire(Counted(&dropped)).unwrap();
            assert_eq!(last.generation, u32::MAX);
            drop(pool.release(last));
            assert_eq!(pool.len(), 0);
            assert!(pool.is_empty());
            assert_eq!(pool.capacity(), 2);
            assert_eq!(pool.free.len(), 1);
            assert!(!pool.occupied[0]);
            assert_eq!(pool.generations[0], u32::MAX);
            let survivor = pool.acquire(Counted(&dropped)).unwrap();
            assert_ne!(survivor.index(), last.index());
            assert_eq!(pool.len(), 1);
            let error = pool.acquire(Counted(&dropped)).unwrap_err();
            assert_eq!(error.what, "retiring handles");
            assert_eq!(error.capacity, 2);
            assert_eq!(dropped.load(Ordering::Relaxed), 3);
        });
        drop(pool);
        assert_eq!(dropped.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn foreign_handles_cannot_read_mutate_or_release_another_pool() {
        let mut budget = Budget::new(4096);
        let mut left = Pool::new(&mut budget, "left owner", 1).unwrap();
        let mut right = Pool::new(&mut budget, "right owner", 1).unwrap();
        let first = left.acquire(1u64).unwrap();
        let second = right.acquire(2u64).unwrap();
        assert_eq!(first.index(), second.index());
        assert_eq!(first.generation, second.generation);
        assert_ne!(first.pool, second.pool);
        assert!(std::panic::catch_unwind(|| right.get(first)).is_err());
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                right.get_mut(first);
            }))
            .is_err()
        );
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| right.release(first)))
                .is_err()
        );
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(*left.get(first), 1);
            assert_eq!(*right.get(second), 2);
            assert_eq!(right.len(), 1);
            assert_eq!(right.release(second), 2);
            assert_eq!(left.release(first), 1);
        });
    }

    #[test]
    fn exhausted_pool_identity_counter_returns_a_named_startup_error() {
        let counter = AtomicUsize::new(usize::MAX - 1);
        let budget = Budget::new(1024);
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                allocate_pool_identity(&counter, &budget).unwrap(),
                usize::MAX - 1
            );
            let error = allocate_pool_identity(&counter, &budget).unwrap_err();
            assert_eq!(error.what, "pool handle identities");
            assert_eq!(counter.load(Ordering::Relaxed), usize::MAX);
            assert_eq!(budget.remaining(), 1024);
        });
    }
}
