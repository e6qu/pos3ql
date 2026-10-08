//! Statement-owned immutable table definitions from a startup-sized pool.

use core::cell::{Cell, UnsafeCell};
use core::mem::{MaybeUninit, size_of};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::{DatabaseOid, Storage, TableDef, table_slot_capacity};
use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

struct Image {
    next: Option<usize>,
    table: usize,
    transaction: u32,
    database: DatabaseOid,
    created_at: u64,
    definition: TableDef,
}

struct ImageCell {
    image: UnsafeCell<MaybeUninit<Image>>,
}

// SAFETY: allocation is serialized. A cell is written only when unoccupied,
// after its previous owner's release. Published payloads are immutable until
// their owning TableDefinitionImages drops; references borrow that owner.
unsafe impl Sync for ImageCell {}

pub(super) struct TableDefinitionImagePool {
    cells: Box<[ImageCell]>,
    occupied: Box<[AtomicBool]>,
    allocation: Mutex<()>,
}

impl TableDefinitionImagePool {
    fn capacity(config: &Config) -> usize {
        table_slot_capacity(config)
            .saturating_mul(config.max_catalog_versions_per_object.saturating_add(1))
            .saturating_mul(config.query_workspace_slots)
    }

    fn shared_bytes() -> usize {
        size_of::<Self>() + 2 * size_of::<AtomicUsize>()
    }

    pub(super) fn budget_bytes(config: &Config) -> usize {
        Self::capacity(config)
            .saturating_mul(size_of::<ImageCell>() + size_of::<AtomicBool>())
            .saturating_add(Self::shared_bytes())
    }

    pub(super) fn new(config: &Config, budget: &mut Budget) -> Result<Arc<Self>, BudgetError> {
        let capacity = Self::capacity(config);
        budget.draw(Self::shared_bytes(), "table definition image owner")?;
        budget.draw_array(
            capacity,
            size_of::<AtomicBool>(),
            "table definition image occupancy",
        )?;
        budget.draw_array(capacity, size_of::<ImageCell>(), "table definition images")?;
        let mut occupied = Box::<[AtomicBool]>::new_uninit_slice(capacity);
        for flag in &mut occupied {
            flag.write(AtomicBool::new(false));
        }
        // SAFETY: every atomic flag was initialized above. Keeping these
        // flags dense avoids touching a page in each reserved wide image.
        let occupied = unsafe { occupied.assume_init() };
        let cells = Box::<[ImageCell]>::new_uninit_slice(capacity);
        // SAFETY: ImageCell's only field is UnsafeCell<MaybeUninit<Image>>,
        // whose payload explicitly permits uninitialized bytes. Image pages
        // remain untouched until a reader captures a definition into them.
        let cells = unsafe { cells.assume_init() };
        Ok(Arc::new(Self {
            cells,
            occupied,
            allocation: Mutex::new(()),
        }))
    }
}

/// A statement retains each (table, transaction) image once. Nested statements
/// get independent owners. The owner keeps the pool alive even after Storage
/// drops, without allocating or retaining a borrow of mutable storage.
pub(crate) struct TableDefinitionImages {
    pool: Arc<TableDefinitionImagePool>,
    head: Cell<Option<usize>>,
}

impl TableDefinitionImages {
    pub(super) fn new(pool: &Arc<TableDefinitionImagePool>) -> Self {
        Self {
            pool: Arc::clone(pool),
            head: Cell::new(None),
        }
    }

    pub(crate) fn retained_definition(&self, table: usize, transaction: u32) -> Option<&TableDef> {
        let mut slot = self.head.get();
        while let Some(index) = slot {
            // SAFETY: this owner's immutable chain remains occupied until Drop.
            let image = unsafe { (*self.pool.cells[index].image.get()).assume_init_ref() };
            if image.table == table && image.transaction == transaction {
                return Some(&image.definition);
            }
            slot = image.next;
        }
        None
    }

    pub(crate) fn definition<'a>(
        &'a self,
        storage: &Storage,
        table: usize,
        transaction: u32,
    ) -> Result<&'a TableDef, SqlError> {
        if !Arc::ptr_eq(&self.pool, &storage.table_definition_images) {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "table definition reader belongs to different storage"
            ));
        }
        let source = storage.table(table);
        let mut slot = self.head.get();
        while let Some(index) = slot {
            // SAFETY: this owner retains every cell in its immutable chain.
            // Adding a new head never modifies a previously captured image.
            let image = unsafe { (*self.pool.cells[index].image.get()).assume_init_ref() };
            if image.table == table && image.transaction == transaction {
                if image.database != source.database || image.created_at != source.created_at {
                    return Err(sql_err!(
                        sqlstate::SERIALIZATION_FAILURE,
                        "table identity changed while its definition image was retained"
                    ));
                }
                return Ok(&image.definition);
            }
            slot = image.next;
        }
        let _allocation = self
            .pool
            .allocation
            .lock()
            .expect("table definition image lock poisoned");
        let Some(index) = self
            .pool
            .occupied
            .iter()
            .position(|flag| !flag.load(Ordering::Acquire))
        else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "table definition image pool is exhausted (capacity {})",
                self.pool.cells.len()
            ));
        };
        let cell = &self.pool.cells[index];
        let target = cell.image.get().cast::<Image>();
        let definition = storage.table_def(table, transaction);
        // SAFETY: the allocation lock gives this writer exclusive ownership
        // of the free cell. Acquire observes the previous owner's release.
        // Copy directly into the reserved slot instead of a wide stack image.
        unsafe {
            core::ptr::addr_of_mut!((*target).next).write(self.head.get());
            core::ptr::addr_of_mut!((*target).table).write(table);
            core::ptr::addr_of_mut!((*target).transaction).write(transaction);
            core::ptr::addr_of_mut!((*target).database).write(source.database);
            core::ptr::addr_of_mut!((*target).created_at).write(source.created_at);
            core::ptr::copy_nonoverlapping(
                definition,
                core::ptr::addr_of_mut!((*target).definition),
                1,
            );
        }
        self.pool.occupied[index].store(true, Ordering::Release);
        self.head.set(Some(index));
        // SAFETY: every field is initialized and this owner now retains the
        // immutable image. The reference cannot outlive the owner's borrow.
        Ok(unsafe { &(*target).definition })
    }
}

impl Drop for TableDefinitionImages {
    fn drop(&mut self) {
        let mut slot = self.head.get();
        while let Some(index) = slot {
            let cell = &self.pool.cells[index];
            // SAFETY: Drop has exclusive access to the owner, so all returned
            // references are finished before its cells become reusable.
            slot = unsafe { (*cell.image.get()).assume_init_ref().next };
            self.pool.occupied[index].store(false, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_definition_images_charge_exact_startup_capacity() {
        let mut config = Config::default_dev();
        config.max_tables = 2;
        let bytes = TableDefinitionImagePool::budget_bytes(&config);
        let mut budget = Budget::new(bytes);
        let pool = TableDefinitionImagePool::new(&config, &mut budget).unwrap();
        assert_eq!(budget.used(), bytes);
        assert_eq!(
            pool.cells.len(),
            TableDefinitionImagePool::capacity(&config)
        );
        let mut too_small = Budget::new(bytes - 1);
        let error = match TableDefinitionImagePool::new(&config, &mut too_small) {
            Ok(_) => panic!("undersized definition budget must fail"),
            Err(error) => error,
        };
        assert_eq!(error.what, "table definition images");
        assert_eq!(error.requested, pool.cells.len() * size_of::<ImageCell>());
    }
}
