//! Immutable, statement-owned definition images in a startup-sized pool.

use core::cell::{Cell, UnsafeCell};
use core::mem::{MaybeUninit, size_of};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use {crate::mem::mutex::Mutex, std::sync::Arc};

use crate::mem::budget::{Budget, BudgetError};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

pub(super) trait DefinitionKey: Copy {
    fn index(self) -> usize;
}

struct Image<Key: DefinitionKey, Definition> {
    next: Option<usize>,
    key: Key,
    definition: Definition,
}

struct ImageCell<Key: DefinitionKey, Definition>(UnsafeCell<MaybeUninit<Image<Key, Definition>>>);

// SAFETY: the allocation mutex serializes initialization and destruction.
// Published cells remain immutable until their unique chain owner drops.
unsafe impl<Key: DefinitionKey + Send + Sync, Definition: Send + Sync> Sync
    for ImageCell<Key, Definition>
{
}

pub(super) struct DefinitionImagePool<Key: DefinitionKey, Definition> {
    cells: Box<[ImageCell<Key, Definition>]>,
    occupied: Box<[AtomicBool]>,
    retained: Box<[AtomicUsize]>,
    allocation: Mutex<()>,
    name: &'static str,
}

impl<Key: DefinitionKey, Definition> DefinitionImagePool<Key, Definition> {
    fn control_bytes() -> usize {
        size_of::<Arc<Self>>() + size_of::<Self>() + 2 * size_of::<AtomicUsize>()
    }

    pub(super) fn budget_bytes(capacity: usize, keys: usize) -> usize {
        capacity
            .saturating_mul(size_of::<ImageCell<Key, Definition>>() + size_of::<AtomicBool>())
            .saturating_add(Self::control_bytes())
            .saturating_add(keys.saturating_mul(size_of::<AtomicUsize>()))
    }

    pub(super) fn new(
        budget: &mut Budget,
        capacity: usize,
        keys: usize,
        name: &'static str,
    ) -> Result<Arc<Self>, BudgetError> {
        budget.draw(Self::control_bytes(), name)?;
        budget.draw_array(capacity, size_of::<AtomicBool>(), name)?;
        budget.draw_array(capacity, size_of::<ImageCell<Key, Definition>>(), name)?;
        budget.draw_array(keys, size_of::<AtomicUsize>(), name)?;
        let mut occupied = Box::<[AtomicBool]>::new_uninit_slice(capacity);
        for flag in &mut occupied {
            flag.write(AtomicBool::new(false));
        }
        // SAFETY: every flag is initialized above. Separate occupancy avoids
        // touching a page in every wide definition reserved at startup.
        let occupied = unsafe { occupied.assume_init() };
        let cells = Box::<[ImageCell<Key, Definition>]>::new_uninit_slice(capacity);
        // SAFETY: the sole payload is UnsafeCell<MaybeUninit<Image<...>>>.
        // Unoccupied cells permit uninitialized bytes and are never read.
        let cells = unsafe { cells.assume_init() };
        let mut retained = Box::<[AtomicUsize]>::new_uninit_slice(keys);
        for readers in &mut retained {
            readers.write(AtomicUsize::new(0));
        }
        // SAFETY: all source counters were initialized above.
        let retained = unsafe { retained.assume_init() };
        Ok(Arc::new(Self {
            cells,
            occupied,
            retained,
            allocation: Mutex::new(()),
            name,
        }))
    }

    pub(super) fn retains(&self, index: usize) -> bool {
        self.retained
            .get(index)
            .is_some_and(|readers| readers.load(Ordering::Acquire) != 0)
    }
}

pub(super) struct DefinitionImages<Key: DefinitionKey, Definition> {
    pool: Arc<DefinitionImagePool<Key, Definition>>,
    head: Cell<Option<usize>>,
}

/// An addressable image. Reading it requires a separate owner that keeps its
/// immutable chain alive; retaining the pool alone does not retain a cell.
pub(super) struct DefinitionImageHandle<Key: DefinitionKey, Definition> {
    pool: Arc<DefinitionImagePool<Key, Definition>>,
    index: usize,
}

impl<Key: DefinitionKey, Definition> DefinitionImageHandle<Key, Definition> {
    pub(super) fn index(&self) -> usize {
        self.index
    }

    /// The caller must retain the chain that owns this initialized cell.
    pub(super) unsafe fn definition(&self) -> &Definition {
        assert!(self.pool.occupied[self.index].load(Ordering::Acquire));
        // SAFETY: the caller retains the immutable occupied image.
        unsafe {
            &(*self.pool.cells[self.index].0.get())
                .assume_init_ref()
                .definition
        }
    }

    /// The caller must retain the chain that owns this initialized cell.
    pub(super) unsafe fn key(&self) -> Key {
        assert!(self.pool.occupied[self.index].load(Ordering::Acquire));
        // SAFETY: the caller retains the immutable occupied image.
        unsafe { (*self.pool.cells[self.index].0.get()).assume_init_ref().key }
    }
}

impl<Key: DefinitionKey, Definition> DefinitionImages<Key, Definition> {
    pub(super) fn new(pool: &Arc<DefinitionImagePool<Key, Definition>>) -> Self {
        Self {
            pool: Arc::clone(pool),
            head: Cell::new(None),
        }
    }

    pub(super) fn belongs_to(&self, pool: &Arc<DefinitionImagePool<Key, Definition>>) -> bool {
        Arc::ptr_eq(&self.pool, pool)
    }

    pub(super) fn find(
        &self,
        mut matches: impl FnMut(&Key, &Definition) -> bool,
    ) -> Option<(usize, &Key, &Definition)> {
        let mut slot = self.head.get();
        while let Some(index) = slot {
            // SAFETY: this owner's immutable chain stays occupied until Drop.
            let image = unsafe { (*self.pool.cells[index].0.get()).assume_init_ref() };
            if matches(&image.key, &image.definition) {
                return Some((index, &image.key, &image.definition));
            }
            slot = image.next;
        }
        None
    }

    pub(super) fn handle(&self, definition: &Definition) -> DefinitionImageHandle<Key, Definition> {
        let index = self
            .find(|_, value| core::ptr::eq(value, definition))
            .expect("definition belongs to its immutable chain")
            .0;
        self.handle_at(index)
    }

    pub(super) fn handle_at(&self, index: usize) -> DefinitionImageHandle<Key, Definition> {
        DefinitionImageHandle {
            pool: Arc::clone(&self.pool),
            index,
        }
    }

    fn insert_with(
        &self,
        key: Key,
        initialize: impl FnOnce(*mut Definition),
    ) -> Result<&Definition, SqlError> {
        let _allocation = self
            .pool
            .allocation
            .lock()
            .expect("definition image lock poisoned");
        let retained = &self.pool.retained[key.index()];
        let readers = retained
            .load(Ordering::Acquire)
            .checked_add(1)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "{} reader count is exhausted",
                    self.pool.name
                )
            })?;
        let index = self
            .pool
            .occupied
            .iter()
            .position(|flag| !flag.load(Ordering::Acquire))
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "{} is exhausted (capacity {})",
                    self.pool.name,
                    self.pool.cells.len()
                )
            })?;
        let target = self.pool.cells[index]
            .0
            .get()
            .cast::<Image<Key, Definition>>();
        // SAFETY: the allocation mutex owns this unoccupied cell. Both callers
        // initialize Definition exactly once, without a fallible callback.
        unsafe {
            core::ptr::addr_of_mut!((*target).next).write(self.head.get());
            core::ptr::addr_of_mut!((*target).key).write(key);
            initialize(core::ptr::addr_of_mut!((*target).definition));
        }
        retained.store(readers, Ordering::Release);
        self.pool.occupied[index].store(true, Ordering::Release);
        self.head.set(Some(index));
        // SAFETY: the initialized image belongs to this immutable chain. A
        // returned borrow prevents the owner from dropping and freeing it.
        Ok(unsafe { &(*target).definition })
    }

    pub(super) fn insert(&self, key: Key, definition: Definition) -> Result<&Definition, SqlError> {
        self.insert_with(key, |target| {
            // SAFETY: insert_with supplies a free, correctly aligned payload.
            unsafe {
                target.write(definition);
            }
        })
    }

    pub(super) fn copy(
        &self,
        key: Key,
        definition: &Definition,
        select: impl FnOnce(&mut Definition),
    ) -> Result<&Definition, SqlError>
    where
        Definition: Copy,
    {
        self.insert_with(key, |target| {
            // SAFETY: source and startup cell are distinct valid allocations.
            // Copy directly so wide definitions do not grow a worker's stack.
            unsafe {
                core::ptr::copy_nonoverlapping(definition, target, 1);
                select(&mut *target);
            }
        })
    }
}

impl<Key: DefinitionKey, Definition> Drop for DefinitionImages<Key, Definition> {
    fn drop(&mut self) {
        let _allocation = self
            .pool
            .allocation
            .lock()
            .expect("definition image lock poisoned");
        let mut slot = self.head.get();
        while let Some(index) = slot {
            let target = self.pool.cells[index]
                .0
                .get()
                .cast::<Image<Key, Definition>>();
            // SAFETY: Drop owns the chain exclusively. No returned borrow is
            // live. Destroy owned payloads before publishing the cell as free.
            unsafe {
                slot = (*target).next;
                let retained = &self.pool.retained[(*target).key.index()];
                core::ptr::drop_in_place(target);
                let readers = retained
                    .load(Ordering::Acquire)
                    .checked_sub(1)
                    .expect("definition image retains its source slot");
                retained.store(readers, Ordering::Release);
            }
            self.pool.occupied[index].store(false, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy)]
    struct Key(usize);
    impl DefinitionKey for Key {
        fn index(self) -> usize {
            self.0
        }
    }

    #[test]
    fn definition_images_reserve_exact_bytes_and_fail_before_occupying_a_cell() {
        let bytes = DefinitionImagePool::<Key, u64>::budget_bytes(2, 1);
        let mut budget = Budget::new(bytes);
        let pool = DefinitionImagePool::new(&mut budget, 2, 1, "test definition images").unwrap();
        assert_eq!(budget.remaining(), 0);
        let owner = DefinitionImages::new(&pool);
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(*owner.insert(Key(0), 17u64).unwrap(), 17);
            assert_eq!(*owner.insert(Key(0), 19u64).unwrap(), 19);
            assert_eq!(
                owner.insert(Key(0), 23u64).unwrap_err().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(pool.retained[0].load(Ordering::Acquire), 2);
            assert!(pool.retains(0));
            drop(owner);
            assert!(!pool.retains(0));
            let reused = DefinitionImages::new(&pool);
            assert_eq!(*reused.insert(Key(0), 29u64).unwrap(), 29);
        });
        let mut too_small = Budget::new(bytes - 1);
        assert!(
            DefinitionImagePool::<Key, u64>::new(&mut too_small, 2, 1, "test definition images")
                .is_err()
        );
    }

    #[test]
    fn definition_images_destroy_owned_payloads_after_the_pool_owner_is_removed() {
        let drops = Arc::new(AtomicUsize::new(0));
        struct Value(Arc<AtomicUsize>);
        impl Drop for Value {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let bytes = DefinitionImagePool::<Key, Value>::budget_bytes(1, 1);
        let mut budget = Budget::new(bytes);
        let pool = DefinitionImagePool::new(&mut budget, 1, 1, "owned definition images").unwrap();
        let images = DefinitionImages::new(&pool);
        crate::mem::guard::forbid_alloc(|| {
            images.insert(Key(0), Value(Arc::clone(&drops))).unwrap();
            drop(pool);
            assert_eq!(drops.load(Ordering::Relaxed), 0);
            assert_eq!(
                images
                    .find(|key, _| key.0 == 0)
                    .unwrap()
                    .2
                    .0
                    .load(Ordering::Relaxed),
                0
            );
            drop(images);
            assert_eq!(drops.load(Ordering::Relaxed), 1);
        });
    }

    #[test]
    fn definition_image_handles_keep_addresses_while_the_owning_chain_grows() {
        let mut budget = Budget::new(DefinitionImagePool::<Key, u64>::budget_bytes(2, 1));
        let pool = DefinitionImagePool::new(&mut budget, 2, 1, "handle images").unwrap();
        let owner = DefinitionImages::new(&pool);
        crate::mem::guard::forbid_alloc(|| {
            let definition = owner.insert(Key(0), 17).unwrap();
            let handle = owner.handle(definition);
            drop(pool);
            owner.insert(Key(0), 19).unwrap();
            // SAFETY: owner retains this exact chain through both reads.
            unsafe {
                assert_eq!(*handle.definition(), 17);
                assert_eq!(handle.key().0, 0);
            }
            assert_eq!(*definition, 17);
            drop(handle);
            drop(owner);
        });
    }
}
