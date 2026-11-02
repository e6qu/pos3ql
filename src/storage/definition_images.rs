//! Statement-owned table and synthetic execution schemas in one fixed startup pool.

use core::cell::{Cell, UnsafeCell};
use core::mem::{MaybeUninit, size_of};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use {crate::mem::mutex::Mutex, std::sync::Arc};

use super::table_definitions::TableIdentity;
use super::{Storage, StorageSetupError, TableDef};
use crate::config::Config;
use crate::mem::budget::Budget;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

static NEXT_IMAGE_POOL: AtomicU64 = AtomicU64::new(1);

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

#[expect(
    clippy::large_enum_variant,
    reason = "fixed startup image cells retain physical headers without a runtime allocation"
)]
enum ImagePayload {
    Table {
        source: TableSource,
        definition: TableDef,
    },
    Relation {
        schema: super::SqlName,
        name: super::SqlName,
        n_columns: usize,
        columns: crate::mem::lease_pool::SharedSlice<super::ColumnMeta>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DefinitionImageId {
    pool: u64,
    index: usize,
    serial: u64,
}

struct Image {
    next: Option<usize>,
    serial: u64,
    payload: ImagePayload,
}

struct ImageCell {
    image: UnsafeCell<MaybeUninit<Image>>,
}

// SAFETY: allocation is serialized. A cell is written only when unoccupied,
// after its previous owner's release. Published payloads are immutable until
// their owning TableDefinitionImages drops; references borrow that owner.
unsafe impl Sync for ImageCell {}

pub(super) struct TableDefinitionImagePool {
    identity: u64,
    cells: Box<[ImageCell]>,
    occupied: Box<[AtomicBool]>,
    allocation: Mutex<u64>,
    metadata: Arc<crate::mem::lease_pool::LeasePool>,
}

struct ImageReservation<'a> {
    pool: &'a TableDefinitionImagePool,
    index: usize,
    serial: u64,
    installed: bool,
}

impl ImageReservation<'_> {
    fn install(mut self, next: Option<usize>, payload: ImagePayload) -> DefinitionImageId {
        // This reservation exclusively owns the occupied, unpublished cell.
        unsafe {
            (*self.pool.cells[self.index].image.get()).write(Image {
                next,
                serial: self.serial,
                payload,
            })
        };
        self.installed = true;
        DefinitionImageId {
            pool: self.pool.identity,
            index: self.index,
            serial: self.serial,
        }
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
    ) -> Result<Arc<Self>, StorageSetupError> {
        let identity = Self::reserve_identity(&NEXT_IMAGE_POOL)?;
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
            identity,
            cells,
            occupied,
            allocation: Mutex::new(1),
            metadata: Arc::clone(metadata),
        }))
    }

    fn reserve_identity(counter: &AtomicU64) -> Result<u64, StorageSetupError> {
        counter
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| {
                StorageSetupError::Metadata(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "table definition image pool identities exhausted"
                ))
            })
    }

    fn reserve(&self) -> Result<ImageReservation<'_>, SqlError> {
        let mut allocation = self
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
        let serial = *allocation;
        *allocation = serial.checked_add(1).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "table definition image reservation identities exhausted"
            )
        })?;
        self.occupied[index].store(true, Ordering::Release);
        Ok(ImageReservation {
            pool: self,
            index,
            serial,
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
                if let ImagePayload::Table { source, definition } = &image.payload {
                    return Some((source.target, definition));
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
            if let ImagePayload::Relation {
                schema,
                name,
                n_columns,
                columns,
            } = &image.payload
                && *schema == relation.schema
                && *name == relation.name
                && *n_columns == relation.n_columns
                && columns.as_slice() == relation.columns
            {
                return Ok(DefinitionImageId {
                    pool: self.pool.identity,
                    index,
                    serial: image.serial,
                });
            }
            slot = image.next;
        }
        let reservation = self.pool.reserve()?;
        let columns = self
            .pool
            .metadata
            .copy_slice(relation.columns)
            .map_err(|error| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "{}", error))?;
        let identity = reservation.install(
            self.head.get(),
            ImagePayload::Relation {
                schema: relation.schema,
                name: relation.name,
                n_columns: relation.n_columns,
                columns,
            },
        );
        self.head.set(Some(identity.index));
        Ok(identity)
    }

    pub(crate) fn retained_relation(
        &self,
        identity: DefinitionImageId,
    ) -> Option<RelationDefinition<'_>> {
        if identity.pool != self.pool.identity {
            return None;
        }
        let mut slot = self.head.get();
        while let Some(index) = slot {
            // SAFETY: references borrow the owner that keeps this entire chain occupied.
            let image = unsafe { (*self.pool.cells[index].image.get()).assume_init_ref() };
            if index == identity.index
                && image.serial == identity.serial
                && let ImagePayload::Relation {
                    schema,
                    name,
                    n_columns,
                    columns,
                } = &image.payload
            {
                return Some(RelationDefinition {
                    schema: *schema,
                    name: *name,
                    n_columns: *n_columns,
                    columns: columns.as_slice(),
                });
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
            if let ImagePayload::Table { source, definition } = &image.payload
                && source.table == table
                && source.transaction == transaction
            {
                return Some(definition);
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
            if let ImagePayload::Table { source, .. } = &image.payload
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
            if let ImagePayload::Table { source, .. } = &image.payload
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
            if let ImagePayload::Table { source, definition } = &image.payload
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
                return Ok(definition);
            }
            slot = image.next;
        }
        let reservation = self.pool.reserve()?;
        // The image allocator lock is released before waiting for the source.
        let definition = storage.table_def(table, transaction);
        let identity = definition.identity();
        let captured = reservation.install(
            self.head.get(),
            ImagePayload::Table {
                source: TableSource {
                    table,
                    transaction,
                    identity,
                    target: storage.table_definition_target(table, transaction, &definition),
                },
                definition: (*definition).clone(),
            },
        );
        let target = self.pool.cells[captured.index].image.get().cast::<Image>();
        self.head.set(Some(captured.index));
        // SAFETY: every field is initialized and this owner now retains the
        // immutable image. The reference cannot outlive the owner's borrow.
        let image = unsafe { &*target };
        let ImagePayload::Table { definition, .. } = &image.payload else {
            unreachable!("table capture installs a physical table payload");
        };
        Ok(definition)
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

    fn metadata_pool(bytes: usize, leases: usize) -> Arc<crate::mem::lease_pool::LeasePool> {
        use crate::mem::lease_pool::{LeasePool, Purpose};
        let bytes = (bytes + 63) & !63;
        let mut budget = Budget::new(LeasePool::budget_bytes(bytes, leases));
        LeasePool::new(&mut budget, Purpose::TABLE_METADATA, bytes, leases).unwrap()
    }

    fn image_pool(
        metadata: &Arc<crate::mem::lease_pool::LeasePool>,
        slots: usize,
    ) -> Arc<TableDefinitionImagePool> {
        let mut config = Config::default_dev();
        config.definition_image_slots = slots;
        let mut budget = Budget::new(TableDefinitionImagePool::budget_bytes(&config));
        TableDefinitionImagePool::new(&config, &mut budget, metadata).unwrap()
    }

    #[test]
    fn synthetic_image_reservation_failure_releases_cell_and_metadata_before_reuse() {
        use crate::storage::{ColumnMeta, SqlName};
        let metadata = metadata_pool(size_of::<ColumnMeta>(), 1);
        let pool = image_pool(&metadata, 1);
        let columns = [ColumnMeta::EMPTY];
        let relation = RelationDefinition {
            schema: SqlName::parse("pg_catalog").unwrap(),
            name: SqlName::parse("bounded").unwrap(),
            n_columns: 1,
            columns: &columns,
        };
        let images = TableDefinitionImages::new(&pool);
        crate::mem::guard::forbid_alloc(|| {
            let held = metadata.lease::<u8>(metadata.max_entries::<u8>()).unwrap();
            assert_eq!(
                images.retain_relation(&relation).unwrap_err().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert!(!pool.occupied[0].load(Ordering::Acquire));
            assert!(pool.allocation.try_lock().is_ok());
            drop(held);
            let identity = images.retain_relation(&relation).unwrap();
            assert_eq!(images.retain_relation(&relation).unwrap(), identity);
            assert!(metadata.lease::<u8>(1).is_err());
            assert_eq!(images.retained_relation(identity).unwrap().columns.len(), 1);
            drop(images);
            assert!(!pool.occupied[0].load(Ordering::Acquire));
            let reclaimed = metadata.lease::<u8>(metadata.max_entries::<u8>()).unwrap();
            drop(reclaimed);
        });
    }

    #[test]
    fn synthetic_image_identifiers_reject_reused_cells_and_foreign_pools() {
        use crate::storage::{ColumnMeta, SqlName};
        let metadata = metadata_pool(2 * size_of::<ColumnMeta>().div_ceil(64) * 64, 2);
        let pool = image_pool(&metadata, 1);
        let other = image_pool(&metadata, 1);
        let columns = [ColumnMeta::EMPTY];
        let relation = RelationDefinition {
            schema: SqlName::parse("pg_catalog").unwrap(),
            name: SqlName::parse("retained").unwrap(),
            n_columns: 1,
            columns: &columns,
        };
        crate::mem::guard::forbid_alloc(|| {
            let first = TableDefinitionImages::new(&pool);
            let prior = first.retain_relation(&relation).unwrap();
            drop(first);
            let second = TableDefinitionImages::new(&pool);
            let current = second.retain_relation(&relation).unwrap();
            assert_eq!(current.index, prior.index);
            assert_ne!(current.serial, prior.serial);
            assert!(second.retained_relation(prior).is_none());
            let foreign = TableDefinitionImages::new(&other);
            let foreign_id = foreign.retain_relation(&relation).unwrap();
            assert_eq!(current.index, foreign_id.index);
            assert_ne!(current.pool, foreign_id.pool);
            assert!(foreign.retained_relation(current).is_none());
            assert!(second.retained_relation(foreign_id).is_none());
            assert_eq!(
                second.retained_relation(current).unwrap().name,
                relation.name
            );
            assert_eq!(
                foreign.retained_relation(foreign_id).unwrap().name,
                relation.name
            );
        });
    }

    #[test]
    fn image_reservation_releases_allocator_before_source_ownership_and_on_unwind() {
        let metadata = metadata_pool(64, 1);
        let pool = image_pool(&metadata, 1);
        crate::mem::guard::forbid_alloc(|| {
            let reservation = pool.reserve().unwrap();
            assert!(pool.occupied[0].load(Ordering::Acquire));
            assert!(pool.allocation.try_lock().is_ok());
            drop(reservation);
            assert!(!pool.occupied[0].load(Ordering::Acquire));
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _reservation = pool.reserve().unwrap();
            panic!("release unpublished image reservation");
        }));
        assert!(result.is_err());
        assert!(!pool.occupied[0].load(Ordering::Acquire));
        assert!(pool.allocation.try_lock().is_ok());
    }

    #[test]
    fn unwinding_relation_owner_drops_schema_pins_before_cell_reuse() {
        use crate::storage::{ColumnMeta, SqlName};
        let metadata = metadata_pool(size_of::<ColumnMeta>(), 1);
        let pool = image_pool(&metadata, 1);
        let columns = [ColumnMeta::EMPTY];
        let relation = RelationDefinition {
            schema: SqlName::parse("pg_catalog").unwrap(),
            name: SqlName::parse("unwind").unwrap(),
            n_columns: 1,
            columns: &columns,
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let images = TableDefinitionImages::new(&pool);
            images.retain_relation(&relation).unwrap();
            panic!("release retained relation image");
        }));
        assert!(result.is_err());
        crate::mem::guard::forbid_alloc(|| {
            assert!(!pool.occupied[0].load(Ordering::Acquire));
            let images = TableDefinitionImages::new(&pool);
            let identity = images.retain_relation(&relation).unwrap();
            assert_eq!(images.retained_relation(identity).unwrap().columns.len(), 1);
        });
    }

    #[test]
    fn image_pool_and_reservation_identities_fail_explicitly_without_wrapping() {
        let counter = AtomicU64::new(u64::MAX - 1);
        let metadata = metadata_pool(64, 1);
        let pool = image_pool(&metadata, 1);
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                TableDefinitionImagePool::reserve_identity(&counter).unwrap(),
                u64::MAX - 1
            );
            let error = TableDefinitionImagePool::reserve_identity(&counter).unwrap_err();
            assert!(matches!(error, StorageSetupError::Metadata(error)
                if error.sqlstate == sqlstate::PROGRAM_LIMIT_EXCEEDED
                    && error.message.as_str().contains("pool identities exhausted")));
            *pool.allocation.lock().unwrap() = u64::MAX - 1;
            let reservation = pool.reserve().unwrap();
            assert_eq!(reservation.serial, u64::MAX - 1);
            drop(reservation);
            let error = match pool.reserve() {
                Ok(_) => panic!("reservation identity must never wrap"),
                Err(error) => error,
            };
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(
                error
                    .message
                    .as_str()
                    .contains("reservation identities exhausted")
            );
            assert!(!pool.occupied[0].load(Ordering::Acquire));
        });
    }

    #[test]
    fn catalog_lifetime_schemas_share_bounded_images_and_release_after_errors() {
        use crate::storage::tests::{make_def, test_budget, test_config};
        use crate::storage::{ColType, ColumnMeta, SqlName};
        let mut config = test_config();
        config.max_tables = 1;
        config.max_catalog_versions_per_object = 1;
        config.query_workspace_slots = 1;
        config.definition_image_slots = 4;
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
        let original = crate::mem::guard::forbid_alloc(|| {
            images.definition(&storage, table, 0).unwrap();
            let original = images.retain_relation(&schema).unwrap();
            assert_eq!(images.retain_relation(&schema).unwrap(), original);
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
            assert_eq!(retained.columns().len(), 1);
            assert_eq!(retained.columns.len(), 2);
            assert_eq!(retained.columns[1].name.as_str(), "tableoid");
            assert_eq!(retained.columns[1].ctype, ColType::Oid);
            assert_eq!(
                images.retained_definition(table, 0).unwrap().name.as_str(),
                "ordinary"
            );
            original
        });
        drop(storage);
        assert_eq!(images.iter().count(), 1);
        assert_eq!(
            images.retained_relation(original).unwrap().columns[1]
                .name
                .as_str(),
            "tableoid"
        );
        let retained = images.retained_definition(table, 0).unwrap();
        assert_eq!(retained.not_null_names.len(), retained.n_columns);
        assert_eq!(retained.columns()[0].name.as_str(), "value");
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
        let metadata = metadata_pool(64, 1);
        let mut budget = Budget::new(bytes);
        let pool = TableDefinitionImagePool::new(&config, &mut budget, &metadata).unwrap();
        assert_eq!(budget.used(), bytes);
        assert_eq!(
            pool.cells.len(),
            TableDefinitionImagePool::capacity(&config)
        );
        let mut too_small = Budget::new(bytes - 1);
        let error = match TableDefinitionImagePool::new(&config, &mut too_small, &metadata) {
            Ok(_) => panic!("undersized definition budget must fail"),
            Err(StorageSetupError::Budget(error)) => error,
            Err(error) => panic!("unexpected image setup error: {error}"),
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
        let mut pending =
            super::super::TableDefBuilder::from_definition(&storage.table_def(table, 0));
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
