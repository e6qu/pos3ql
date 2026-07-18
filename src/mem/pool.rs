//! A fixed-size object pool with generational handles.
//!
//! Handles are indices tagged with a generation; using a handle after its
//! slot was released panics with the pool's name rather than silently
//! reading another object's data (the TigerBeetle-style answer to
//! use-after-free in index-based designs).

use core::fmt;
use core::mem::MaybeUninit;

use super::budget::{Budget, BudgetError};
use super::fixed_vec::FixedVec;

pub struct Pool<T> {
    what: &'static str,
    values: Box<[MaybeUninit<T>]>,
    generations: Box<[u32]>,
    occupied: Box<[bool]>,
    free: FixedVec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Handle {
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

impl<T> Pool<T> {
    pub fn new(
        budget: &mut Budget,
        what: &'static str,
        capacity: usize,
    ) -> Result<Self, BudgetError> {
        assert!(capacity <= u32::MAX as usize, "pool '{what}' capacity overflows u32");
        budget.draw_array(capacity, size_of::<T>() + size_of::<u32>() + 1, what)?;
        let mut free = FixedVec::new(budget, what, capacity)?;
        for index in (0..capacity as u32).rev() {
            free.push(index).expect("free list sized to capacity");
        }
        Ok(Self {
            what,
            values: Box::new_uninit_slice(capacity),
            generations: vec![0; capacity].into_boxed_slice(),
            occupied: vec![false; capacity].into_boxed_slice(),
            free,
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
        Ok(Handle {
            index,
            generation: self.generations[i],
        })
    }

    pub fn release(&mut self, handle: Handle) -> T {
        let i = self.check(handle);
        self.occupied[i] = false;
        self.generations[i] = self.generations[i].wrapping_add(1);
        self.free.push(handle.index).expect("free list sized to capacity");
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
        self.values.len() - self.free.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn capacity(&self) -> usize {
        self.values.len()
    }

    /// Live handles in unspecified order.
    pub fn iter_handles(&self) -> impl Iterator<Item = Handle> + '_ {
        self.occupied
            .iter()
            .enumerate()
            .filter(|(_, occupied)| **occupied)
            .map(|(i, _)| Handle {
                index: i as u32,
                generation: self.generations[i],
            })
    }

    fn check(&self, handle: Handle) -> usize {
        let i = handle.index as usize;
        assert!(
            i < self.values.len()
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

#[cfg(test)]
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
