//! A vector whose capacity is fixed at construction. Pushing past capacity
//! is an error naming the structure, never a reallocation.

use core::fmt;
use core::mem::MaybeUninit;
use core::ops::{Deref, DerefMut};

use super::budget::{Budget, BudgetError};

pub struct FixedVec<T> {
    what: &'static str,
    buf: Box<[MaybeUninit<T>]>,
    len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapacityError {
    pub what: &'static str,
    pub capacity: usize,
}

impl fmt::Display for CapacityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "'{}' is full (capacity {})", self.what, self.capacity)
    }
}

impl std::error::Error for CapacityError {}

impl<T> FixedVec<T> {
    pub fn new(
        budget: &mut Budget,
        what: &'static str,
        capacity: usize,
    ) -> Result<Self, BudgetError> {
        budget.draw_array(capacity, size_of::<T>(), what)?;
        Ok(Self {
            what,
            buf: Box::new_uninit_slice(capacity),
            len: 0,
        })
    }

    pub fn push(&mut self, value: T) -> Result<(), CapacityError> {
        if self.len == self.buf.len() {
            return Err(CapacityError {
                what: self.what,
                capacity: self.buf.len(),
            });
        }
        self.buf[self.len].write(value);
        self.len += 1;
        Ok(())
    }

    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        Some(unsafe { self.buf[self.len].assume_init_read() })
    }

    /// Removes the element at `index` by swapping the last element into its
    /// place. O(1), order not preserved. Panics if out of bounds.
    pub fn swap_remove(&mut self, index: usize) -> T {
        assert!(
            index < self.len,
            "'{}': swap_remove index {} out of bounds (len {})",
            self.what,
            index,
            self.len
        );
        let last = self.len - 1;
        self.as_mut_slice().swap(index, last);
        self.pop().unwrap()
    }

    pub fn clear(&mut self) {
        while self.pop().is_some() {}
    }

    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    pub fn as_slice(&self) -> &[T] {
        unsafe { core::slice::from_raw_parts(self.buf.as_ptr().cast::<T>(), self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [T] {
        unsafe { core::slice::from_raw_parts_mut(self.buf.as_mut_ptr().cast::<T>(), self.len) }
    }
}

impl<T> Deref for FixedVec<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T> DerefMut for FixedVec<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<T> Drop for FixedVec<T> {
    fn drop(&mut self) {
        self.clear();
    }
}

impl<T: fmt::Debug> fmt::Debug for FixedVec<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_pop_within_capacity() {
        let mut budget = Budget::new(1024);
        let mut v: FixedVec<u32> = FixedVec::new(&mut budget, "test", 4).unwrap();
        for i in 0..4 {
            v.push(i).unwrap();
        }
        assert_eq!(v.as_slice(), &[0, 1, 2, 3]);
        assert_eq!(v.pop(), Some(3));
        assert_eq!(v.len(), 3);
    }

    #[test]
    fn push_past_capacity_fails_loudly() {
        let mut budget = Budget::new(1024);
        let mut v: FixedVec<u32> = FixedVec::new(&mut budget, "conn_slots", 2).unwrap();
        v.push(1).unwrap();
        v.push(2).unwrap();
        let err = v.push(3).unwrap_err();
        assert_eq!(err.what, "conn_slots");
        assert_eq!(err.capacity, 2);
    }

    #[test]
    fn construction_draws_budget() {
        let mut budget = Budget::new(15);
        let err = FixedVec::<u32>::new(&mut budget, "too_big", 4).unwrap_err();
        assert_eq!(err.what, "too_big");
        assert!(FixedVec::<u32>::new(&mut budget, "fits", 3).is_ok());
    }

    #[test]
    fn drop_runs_element_destructors() {
        use std::rc::Rc;
        let mut budget = Budget::new(1024);
        let marker = Rc::new(());
        {
            let mut v = FixedVec::new(&mut budget, "test", 4).unwrap();
            v.push(Rc::clone(&marker)).unwrap();
            v.push(Rc::clone(&marker)).unwrap();
            assert_eq!(Rc::strong_count(&marker), 3);
        }
        assert_eq!(Rc::strong_count(&marker), 1);
    }

    #[test]
    fn swap_remove_is_unordered_removal() {
        let mut budget = Budget::new(1024);
        let mut v: FixedVec<u32> = FixedVec::new(&mut budget, "test", 4).unwrap();
        for i in 0..4 {
            v.push(i).unwrap();
        }
        assert_eq!(v.swap_remove(1), 1);
        assert_eq!(v.as_slice(), &[0, 3, 2]);
    }

    #[test]
    fn pushes_do_not_allocate() {
        let mut budget = Budget::new(1024);
        let mut v: FixedVec<u64> = FixedVec::new(&mut budget, "test", 64).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            for i in 0..64 {
                v.push(i).unwrap();
            }
        });
        assert_eq!(v.len(), 64);
    }
}
