//! Statement-owned table and synthetic execution schemas in one fixed startup pool.

use core::cell::{Cell, UnsafeCell};
use core::mem::{MaybeUninit, size_of};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use {crate::mem::mutex::Mutex, std::sync::Arc};

use super::table_definitions::TableIdentity;
use super::{Storage, TableDef};
use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

/// Borrowed columns for query schemas; hidden fields may follow the visible prefix.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RelationDefinition<'a> {
    pub(crate) schema: super::SqlName,
    pub(crate) name: super::SqlName,
    pub(crate) n_columns: usize,
    pub(crate) columns: &'a [super::ColumnMeta],
}

impl<'a> RelationDefinition<'a> {
    pub(crate) fn from_definition<B: super::DefinitionBacking>(
        definition: &'a super::TableDefinition<B>,
    ) -> Self {
        Self {
            schema: definition.schema,
            name: definition.name,
            n_columns: definition.n_columns,
            columns: &definition.columns.as_ref()[..definition.n_columns],
        }
    }

    pub(crate) fn columns(&self) -> &'a [super::ColumnMeta] {
        &self.columns[..self.n_columns]
    }

    pub(crate) fn column_index(&self, name: &str) -> Option<usize> {
        self.columns()
            .iter()
            .position(|column| column.name.as_str() == name)
    }

    pub(crate) fn schema(
        &self,
        output: &mut [crate::sql::types::ColType; super::MAX_COLUMNS],
    ) -> usize {
        for (index, column) in self.columns().iter().enumerate() {
            output[index] = column.ctype;
        }
        self.n_columns
    }
}

#[derive(Clone, Copy)]
struct TableSource {
    table: usize,
    transaction: u32,
    identity: TableIdentity,
    target: super::TableDefinitionTarget,
}

#[derive(Clone, Copy)]
enum ImageSource {
    Table(TableSource),
    Relation { width: usize },
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DefinitionImageId(usize);

struct Image {
    next: Option<usize>,
    source: ImageSource,
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
    metadata: Arc<crate::mem::lease_pool::LeasePool>,
}

struct ImageReservation<'a> {
    pool: &'a TableDefinitionImagePool,
    index: usize,
    installed: bool,
}

impl ImageReservation<'_> {
    fn install(mut self, image: Image) -> usize {
        // This reservation exclusively owns the occupied, unpublished cell.
        unsafe { (*self.pool.cells[self.index].image.get()).write(image) };
        self.installed = true;
        self.index
    }
}

impl Drop for ImageReservation<'_> {
    fn drop(&mut self) {
        if !self.installed {
            self.pool.occupied[self.index].store(false, Ordering::Release);
        }
    }
}

impl TableDefinitionImagePool {
    fn capacity(config: &Config) -> usize {
        config.definition_image_slots
    }

    fn shared_bytes() -> usize {
        size_of::<Self>() + 2 * size_of::<AtomicUsize>()
    }

    pub(super) fn budget_bytes(config: &Config) -> usize {
        Self::capacity(config)
            .saturating_mul(size_of::<ImageCell>() + size_of::<AtomicBool>())
            .saturating_add(Self::shared_bytes())
    }

    pub(super) fn new(
        config: &Config,
        budget: &mut Budget,
        metadata: &Arc<crate::mem::lease_pool::LeasePool>,
    ) -> Result<Arc<Self>, BudgetError> {
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
            metadata: Arc::clone(metadata),
        }))
    }

    fn reserve(&self) -> Result<ImageReservation<'_>, SqlError> {
        let _allocation = self
            .allocation
            .lock()
            .expect("table definition image lock poisoned");
        let index = self
            .occupied
            .iter()
            .position(|flag| !flag.load(Ordering::Acquire))
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "table definition image pool is exhausted (capacity {})",
                    self.cells.len()
                )
            })?;
        self.occupied[index].store(true, Ordering::Release);
        Ok(ImageReservation {
            pool: self,
            index,
            installed: false,
        })
    }
}

/// A statement retains each (table, transaction) image and synthetic shape once.
/// Nested statements get independent owners. The owner keeps the pool alive even after Storage
/// drops, without allocating or retaining a borrow of mutable storage.
pub(crate) struct TableDefinitionImages {
    pool: Arc<TableDefinitionImagePool>,
    head: Cell<Option<usize>>,
}

impl Storage {
    /// Consume a copied definition after the live catalog guards release.
    /// Returned values cannot borrow the temporary definition owner.
    pub(crate) fn with_retained_table_definition<R>(
        &self,
        table: usize,
        transaction: u32,
        each: impl FnOnce(&TableDef) -> Result<R, SqlError>,
    ) -> Result<R, SqlError> {
        let images = self.table_definition_images();
        each(images.definition(self, table, transaction)?)
    }
}

impl TableDefinitionImages {
    pub(super) fn new(pool: &Arc<TableDefinitionImagePool>) -> Self {
        Self {
            pool: Arc::clone(pool),
            head: Cell::new(None),
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (super::TableDefinitionTarget, &TableDef)> {
        let mut slot = self.head.get();
        core::iter::from_fn(move || {
            loop {
                let index = slot?;
                // SAFETY: the borrowed owner pins every immutable image in this chain.
                let image = unsafe { (*self.pool.cells[index].image.get()).assume_init_ref() };
                slot = image.next;
                if let ImageSource::Table(source) = image.source {
                    return Some((source.target, &image.definition));
                }
            }
        })
    }

    pub(crate) fn retain_relation(
        &self,
        relation: &RelationDefinition<'_>,
    ) -> Result<DefinitionImageId, SqlError> {
        let width = relation.columns.len();
        if relation.n_columns > width || width > super::MAX_COLUMNS {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "invalid synthetic relation width"
            ));
        }
        let mut slot = self.head.get();
        while let Some(index) = slot {
            // SAFETY: this owner pins the chain, including any previously retained schema.
            let image = unsafe { (*self.pool.cells[index].image.get()).assume_init_ref() };
            if matches!(image.source, ImageSource::Relation { width: saved } if saved == width)
                && image.definition.schema == relation.schema
                && image.definition.name == relation.name
                && image.definition.n_columns == relation.n_columns
                && image.definition.columns[..width] == *relation.columns
            {
                return Ok(DefinitionImageId(index));
            }
            slot = image.next;
        }
        let reservation = self.pool.reserve()?;
        let columns = self
            .pool
            .metadata
            .copy_slice(relation.columns)
            .map_err(|error| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "{}", error))?;
        let index = reservation.install(Image {
            next: self.head.get(),
            source: ImageSource::Relation { width },
            definition: TableDef {
                schema: relation.schema,
                name: relation.name,
                n_columns: relation.n_columns,
                columns,
                ..TableDef::empty()
            },
        });
        self.head.set(Some(index));
        Ok(DefinitionImageId(index))
    }

    pub(crate) fn retained_relation(&self, identity: DefinitionImageId) -> Option<&TableDef> {
        let mut slot = self.head.get();
        while let Some(index) = slot {
            // SAFETY: references borrow the owner that keeps this entire chain occupied.
            let image = unsafe { (*self.pool.cells[index].image.get()).assume_init_ref() };
            if index == identity.0 && matches!(image.source, ImageSource::Relation { .. }) {
                return Some(&image.definition);
            }
            slot = image.next;
        }
        None
    }

    /// Read a captured image without reacquiring the live table identity.
    pub(crate) fn retained_definition(&self, table: usize, transaction: u32) -> Option<&TableDef> {
        let mut slot = self.head.get();
        while let Some(index) = slot {
            // SAFETY: this owner's immutable chain remains occupied until Drop.
            let image = unsafe { (*self.pool.cells[index].image.get()).assume_init_ref() };
            if let ImageSource::Table(source) = image.source
                && source.table == table
                && source.transaction == transaction
            {
                return Some(&image.definition);
            }
            slot = image.next;
        }
        None
    }

    pub(crate) fn retained_identity(
        &self,
        table: usize,
        transaction: u32,
    ) -> Option<TableIdentity> {
        let mut slot = self.head.get();
        while let Some(index) = slot {
            // SAFETY: the owner retains this immutable image until Drop.
            let image = unsafe { (*self.pool.cells[index].image.get()).assume_init_ref() };
            if let ImageSource::Table(source) = image.source
                && source.table == table
                && source.transaction == transaction
            {
                return Some(source.identity);
            }
            slot = image.next;
        }
        None
    }

    pub(crate) fn selection<'a>(
        &'a self,
        storage: &Storage,
        table: usize,
        transaction: u32,
    ) -> Result<(super::TableDefinitionTarget, &'a TableDef), SqlError> {
        let definition = self.definition(storage, table, transaction)?;
        let mut slot = self.head.get();
        while let Some(index) = slot {
            // SAFETY: this owner's immutable chain remains occupied until Drop.
            let image = unsafe { (*self.pool.cells[index].image.get()).assume_init_ref() };
            if let ImageSource::Table(source) = image.source
                && source.table == table
                && source.transaction == transaction
            {
                return Ok((source.target, definition));
            }
            slot = image.next;
        }
        unreachable!("definition capture installs its source before returning")
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
        let selected = storage.table(table).identity();
        let mut slot = self.head.get();
        while let Some(index) = slot {
            // SAFETY: this owner retains every cell in its immutable chain.
            // Adding a new head never modifies a previously captured image.
            let image = unsafe { (*self.pool.cells[index].image.get()).assume_init_ref() };
            if let ImageSource::Table(source) = image.source
                && source.table == table
                && source.transaction == transaction
            {
                if source.identity.database != selected.database
                    || source.identity.created_at != selected.created_at
                {
                    return Err(sql_err!(
                        sqlstate::SERIALIZATION_FAILURE,
                        "table identity changed while its definition image was retained"
                    ));
                }
                return Ok(&image.definition);
            }
            slot = image.next;
        }
        let reservation = self.pool.reserve()?;
        // The image allocator lock is released before waiting for the source.
        let definition = storage.table_def(table, transaction);
        let identity = definition.identity();
        let index = reservation.install(Image {
            next: self.head.get(),
            source: ImageSource::Table(TableSource {
                table,
                transaction,
                identity,
                target: storage.table_definition_target(table, transaction, &definition),
            }),
            definition: (*definition).clone(),
        });
        let target = self.pool.cells[index].image.get().cast::<Image>();
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
            // Release schema pins before this cell can be reserved again.
            unsafe { (*cell.image.get()).assume_init_drop() };
            self.pool.occupied[index].store(false, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_lifetime_schemas_share_bounded_images_and_release_after_errors() {
        use crate::storage::tests::{make_def, test_budget, test_config};
        use crate::storage::{ColType, ColumnMeta, SqlName};
        let mut config = test_config();
        config.max_tables = 1;
        config.max_catalog_versions_per_object = 1;
        config.query_workspace_slots = 1;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let table = storage
            .create_table(make_def("ordinary", &[("value", ColType::Int4, false)]))
            .unwrap();
        let images = storage.table_definition_images();
        let mut columns = [ColumnMeta::EMPTY; 2];
        columns[0].name = SqlName::parse("value").unwrap();
        columns[0].ctype = ColType::Int4;
        columns[1].name = SqlName::parse("tableoid").unwrap();
        columns[1].ctype = ColType::Oid;
        let schema = RelationDefinition {
            schema: SqlName::parse("pg_catalog").unwrap(),
            name: SqlName::parse("sample").unwrap(),
            n_columns: 1,
            columns: &columns,
        };
        crate::mem::guard::forbid_alloc(|| {
            images.definition(&storage, table, 0).unwrap();
            let original = images.retain_relation(&schema).unwrap();
            assert_eq!(images.retain_relation(&schema).unwrap().0, original.0);
            assert_eq!(
                images.iter().count(),
                1,
                "catalog shapes cannot become stored table targets"
            );
            for index in 2..images.pool.cells.len() {
                let name = crate::stack_format!(63, "catalog_{}", index);
                images
                    .retain_relation(&RelationDefinition {
                        name: SqlName::parse(name.as_str()).unwrap(),
                        ..schema
                    })
                    .unwrap();
            }
            let extra = RelationDefinition {
                name: SqlName::parse("overflow").unwrap(),
                ..schema
            };
            assert_eq!(
                images.retain_relation(&extra).unwrap_err().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            let retained = images.retained_relation(original).unwrap();
            assert_eq!(retained.n_columns, 1);
            assert_eq!(retained.columns[1].name.as_str(), "tableoid");
            assert_eq!(retained.columns[1].ctype, ColType::Oid);
            assert_eq!(
                images.retained_definition(table, 0).unwrap().name.as_str(),
                "ordinary"
            );
        });
        drop(storage);
        assert_eq!(images.iter().count(), 1);
        let pool = Arc::clone(&images.pool);
        drop(images);
        crate::mem::guard::forbid_alloc(|| {
            assert!(
                pool.occupied
                    .iter()
                    .all(|flag| !flag.load(Ordering::Acquire))
            );
            let reused = TableDefinitionImages::new(&pool);
            let identity = reused.retain_relation(&schema).unwrap();
            assert_eq!(
                reused.retained_relation(identity).unwrap().columns[0]
                    .name
                    .as_str(),
                "value"
            );
            assert_eq!(reused.iter().count(), 0);
        });
    }

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

    #[test]
    fn retained_definition_callbacks_release_live_and_pending_catalog_guards() {
        use crate::sql::types::ColType;
        use crate::storage::tests::{make_def, test_budget, test_config};
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let table = storage
            .create_table(make_def(
                "description_owner",
                &[("original", ColType::Int4, true)],
            ))
            .unwrap();
        let mut pending = *storage.table_def(table, 0);
        pending.columns[0].name = crate::storage::SqlName::parse("pending_name").unwrap();
        let mut mapping = [None; crate::storage::MAX_COLUMNS];
        mapping[0] = Some(pending.columns[0].name);
        let change = storage
            .write_table_def(
                storage.select_table_definition(table, 7).0,
                pending,
                &mapping,
                false,
            )
            .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let error = storage
                .with_retained_table_definition(table, 7, |definition| {
                    assert_eq!(definition.columns()[0].name.as_str(), "pending_name");
                    assert!(
                        storage.tables[table].definition.test_write().is_ok(),
                        "description callback retained the live table guard"
                    );
                    assert!(
                        storage.pending_table_defs.test_write().is_ok(),
                        "description callback retained the shared pending-schema pool"
                    );
                    Err::<(), _>(sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "description callback error"
                    ))
                })
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::DATA_EXCEPTION);
            storage
                .with_retained_table_definition(table, 7, |definition| {
                    assert_eq!(definition.columns()[0].name.as_str(), "pending_name");
                    assert!(storage.tables[table].definition.test_write().is_ok());
                    assert!(storage.pending_table_defs.test_write().is_ok());
                    Ok(())
                })
                .unwrap();
            storage.rollback_table_def(change);
            storage
                .with_retained_table_definition(table, 7, |definition| {
                    assert_eq!(definition.columns()[0].name.as_str(), "original");
                    Ok(())
                })
                .unwrap();
        });
    }

    #[test]
    fn retained_definition_callback_capacity_failure_skips_callback_and_allows_retry() {
        use crate::sql::types::ColType;
        use crate::storage::tests::{make_def, test_budget, test_config};
        let mut config = test_config();
        config.max_tables = 1;
        config.max_catalog_versions_per_object = 1;
        config.query_workspace_slots = 1;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let table = storage
            .create_table(make_def(
                "description_capacity",
                &[("id", ColType::Int4, true)],
            ))
            .unwrap();
        let capacity = TableDefinitionImagePool::capacity(&config);
        let mut owners = Vec::with_capacity(capacity);
        crate::mem::guard::forbid_alloc(|| {
            for _ in 0..capacity {
                let images = storage.table_definition_images();
                images.definition(&storage, table, 0).unwrap();
                owners.push(images);
            }
            let mut called = false;
            let error = storage
                .with_retained_table_definition(table, 0, |_| {
                    called = true;
                    Ok(())
                })
                .unwrap_err();
            assert!(!called);
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(
                error
                    .message
                    .as_str()
                    .contains("table definition image pool")
            );
            assert!(storage.tables[table].definition.test_write().is_ok());
            assert!(storage.pending_table_defs.test_write().is_ok());
            owners.pop();
            storage
                .with_retained_table_definition(table, 0, |definition| {
                    assert_eq!(definition.columns()[0].name.as_str(), "id");
                    Ok(())
                })
                .unwrap();
            owners.clear();
        });
    }
}
