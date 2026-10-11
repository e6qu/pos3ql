//! Owned immutable SST roots retained until their readers release them.

use core::mem::size_of;
use core::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use super::{
    ColType, MAX_COLUMNS, RelationPersistence, SpillReader, Storage, Table, table_slot_capacity,
};
use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use crate::store::SstHandle;

struct PinnedRoot {
    handle: SstHandle,
    durable: bool,
    readers: NonZeroUsize,
    garbage_retained: bool,
}

pub(super) struct SpillRootRegistry {
    roots: Mutex<FixedVec<PinnedRoot>>,
    reclamation_pending: AtomicBool,
}

impl SpillRootRegistry {
    pub(super) fn capacity(config: &Config) -> usize {
        table_slot_capacity(config)
            .saturating_mul(config.max_spill_generations_per_table)
            .saturating_mul(2)
    }

    pub(super) fn budget_bytes(config: &Config) -> usize {
        size_of::<Self>()
            + 2 * size_of::<std::sync::atomic::AtomicUsize>()
            + Self::capacity(config) * size_of::<PinnedRoot>()
    }

    pub(super) fn new(config: &Config, budget: &mut Budget) -> Result<Arc<Self>, BudgetError> {
        budget.draw(
            size_of::<Self>() + 2 * size_of::<std::sync::atomic::AtomicUsize>(),
            "retained SST root owner",
        )?;
        let roots = FixedVec::new(budget, "retained SST roots", Self::capacity(config))?;
        Ok(Arc::new(Self {
            roots: Mutex::new(roots),
            reclamation_pending: AtomicBool::new(false),
        }))
    }

    fn pin(&self, handles: &[SstHandle], durable: bool) -> Result<(), SqlError> {
        let mut roots = self.roots.lock().expect("retained SST root lock poisoned");
        let mut needed = 0;
        for (index, handle) in handles.iter().enumerate() {
            if !roots
                .iter()
                .any(|root| root.handle == *handle && root.durable == durable)
                && !handles[..index].contains(handle)
            {
                needed += 1;
            }
        }
        for root in roots.iter() {
            let count = handles
                .iter()
                .filter(|handle| **handle == root.handle && root.durable == durable)
                .count();
            if root.readers.checked_add(count).is_none() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "retained SST reader count exhausted"
                ));
            }
        }
        if needed > roots.capacity() - roots.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "retained SST roots exceed the startup capacity ({})",
                roots.capacity()
            ));
        }
        for handle in handles {
            if let Some(root) = roots
                .iter_mut()
                .find(|root| root.handle == *handle && root.durable == durable)
            {
                root.readers = root
                    .readers
                    .checked_add(1)
                    .expect("preflighted reader count");
            } else {
                roots
                    .push(PinnedRoot {
                        handle: *handle,
                        durable,
                        readers: NonZeroUsize::new(1).expect("nonzero"),
                        garbage_retained: false,
                    })
                    .expect("preflighted root capacity");
            }
        }
        Ok(())
    }

    fn release(&self, handles: &[SstHandle], durable: bool) {
        let mut roots = self.roots.lock().expect("retained SST root lock poisoned");
        for handle in handles {
            let position = roots
                .iter()
                .position(|root| root.handle == *handle && root.durable == durable)
                .expect("reader retains its root");
            if let Some(remaining) = NonZeroUsize::new(roots[position].readers.get() - 1) {
                roots[position].readers = remaining;
            } else {
                let retired = roots.swap_remove(position);
                if retired.durable && retired.garbage_retained {
                    self.reclamation_pending.store(true, Ordering::Release);
                }
            }
        }
    }

    pub(super) fn copy_roots(
        &self,
        durable: bool,
        output: &mut Vec<SstHandle>,
    ) -> Result<(), SqlError> {
        let mut roots = self.roots.lock().expect("retained SST root lock poisoned");
        output.clear();
        let count = roots.iter().filter(|root| root.durable == durable).count();
        if count > output.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "retained SST root snapshot exceeds garbage collection scratch ({})",
                output.capacity()
            ));
        }
        for root in roots.iter_mut().filter(|root| root.durable == durable) {
            root.garbage_retained = true;
            output.push(root.handle);
        }
        Ok(())
    }
    pub(super) fn reclamation_pending(&self) -> bool {
        self.reclamation_pending.load(Ordering::Acquire)
    }
    pub(super) fn take_reclamation_pending(&self) -> bool {
        self.reclamation_pending.swap(false, Ordering::AcqRel)
    }
}

pub(super) enum SpillHandles<'a> {
    Installed(&'a [Option<SstHandle>]),
    Retained(&'a [SstHandle]),
}

pub(super) struct SpillRelation<'a> {
    pub(super) persistence: RelationPersistence,
    pub(super) schema: [ColType; MAX_COLUMNS],
    pub(super) n_columns: usize,
    pub(super) handles: SpillHandles<'a>,
}

impl<'a> SpillRelation<'a> {
    pub(super) fn installed(table: &'a Table) -> Self {
        let definition = table.definition();
        let mut schema = [ColType::Bool; MAX_COLUMNS];
        let n_columns = definition.schema(&mut schema);
        Self {
            persistence: definition.persistence,
            schema,
            n_columns,
            handles: SpillHandles::Installed(&table.spill_ssts[..table.n_spill_ssts]),
        }
    }

    pub(super) fn len(&self) -> usize {
        match &self.handles {
            SpillHandles::Installed(handles) => handles.len(),
            SpillHandles::Retained(handles) => handles.len(),
        }
    }

    pub(super) fn handle(&self, member: usize) -> SstHandle {
        match &self.handles {
            SpillHandles::Installed(handles) => handles[member].expect("counted SST member"),
            SpillHandles::Retained(handles) => handles[member],
        }
    }
}

struct QueryRootSlot {
    retained: AtomicBool,
    snapshot: RwLock<SpillGenerationSnapshot>,
}

pub(super) struct QueryRootPool {
    slots: FixedVec<QueryRootSlot>,
}

pub(super) struct QueryRootLease {
    pool: Arc<QueryRootPool>,
    slot: usize,
}

impl QueryRootPool {
    fn slot_count(config: &Config) -> usize {
        config.query_workspace_slots.saturating_mul(super::MAX_ROW_WALK_NESTING)
    }

    pub(super) fn budget_bytes(config: &Config) -> usize {
        size_of::<Self>() + 2 * size_of::<std::sync::atomic::AtomicUsize>()
            + Self::slot_count(config) * (size_of::<QueryRootSlot>()
                + SpillGenerationSnapshot::budget_bytes(config.max_spill_generations_per_table))
    }

    pub(super) fn new(config: &Config, budget: &mut Budget) -> Result<Arc<Self>, BudgetError> {
        budget.draw(size_of::<Self>() + 2 * size_of::<std::sync::atomic::AtomicUsize>(), "query SST root owner")?;
        let count = Self::slot_count(config);
        let mut slots = FixedVec::new(budget, "query SST root slots", count)?;
        for _ in 0..count {
            budget.draw(SpillGenerationSnapshot::budget_bytes(config.max_spill_generations_per_table), "query SST root members")?;
            slots.push(QueryRootSlot {
                retained: AtomicBool::new(false),
                snapshot: RwLock::new(SpillGenerationSnapshot::new(config.max_spill_generations_per_table)),
            }).expect("sized query root slots");
        }
        Ok(Arc::new(Self { slots }))
    }

    pub(super) fn retain(self: &Arc<Self>, storage: &Storage, table: usize, lsn: u64) -> Result<QueryRootLease, SqlError> {
        for (slot, candidate) in self.slots.iter().enumerate() {
            if candidate.retained.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
                continue;
            }
            let lease = QueryRootLease { pool: Arc::clone(self), slot };
            {
                let mut snapshot = candidate.snapshot.write().expect("query SST roots poisoned");
                snapshot.capture(storage, table)?;
                snapshot.commit_lsn = lsn;
            }
            return Ok(lease);
        }
        Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "query SST root slots exhausted ({})", self.slots.len()))
    }
}

impl QueryRootLease {
    /// Only this source owns the slot. Its guard excludes no unrelated reader,
    /// publisher, or retirement; registry ownership ends before object I/O.
    pub(super) fn with_snapshot<T>(&self, read: impl FnOnce(&SpillGenerationSnapshot) -> Result<T, SqlError>) -> Result<T, SqlError> {
        let snapshot = self.pool.slots[self.slot].snapshot.read().expect("query SST roots poisoned");
        read(&snapshot)
    }
}

impl Drop for QueryRootLease {
    fn drop(&mut self) {
        let slot = &self.pool.slots[self.slot];
        slot.snapshot.write().expect("query SST roots poisoned").clear();
        slot.retained.store(false, Ordering::Release);
    }
}

/// The buffers are constructed at startup. Capturing only clones existing Arc
/// owners and copies handles; registry ownership never spans object I/O.
pub(super) struct SpillGenerationSnapshot {
    registry: Option<Arc<SpillRootRegistry>>,
    pub(super) reader: Option<Arc<SpillReader>>,
    handles: Vec<SstHandle>,
    persistence: RelationPersistence,
    schema: [ColType; MAX_COLUMNS],
    n_columns: usize,
    table_slot: usize,
    created_at: u64,
    data_generation: u64,
    pub(super) commit_lsn: u64,
}

impl SpillGenerationSnapshot {
    pub(super) fn budget_bytes(max_generations: usize) -> usize {
        max_generations * size_of::<SstHandle>()
    }

    pub(super) fn new(max_generations: usize) -> Self {
        Self {
            registry: None,
            reader: None,
            handles: Vec::with_capacity(max_generations),
            persistence: RelationPersistence::Permanent,
            schema: [ColType::Bool; MAX_COLUMNS],
            n_columns: 0,
            table_slot: 0,
            created_at: 0,
            data_generation: 0,
            commit_lsn: 0,
        }
    }

    pub(super) fn capture(&mut self, storage: &Storage, table_slot: usize) -> Result<(), SqlError> {
        self.clear();
        let table = storage.table(table_slot);
        if table.n_spill_ssts > self.handles.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "retained SST members exceed checkpoint spill capacity ({})",
                self.handles.capacity()
            ));
        }
        let definition = table.definition();
        self.persistence = definition.persistence;
        self.n_columns = definition.schema(&mut self.schema);
        self.table_slot = table_slot;
        self.created_at = table.created_at();
        self.data_generation = table.generation();
        self.commit_lsn = storage.lsn();
        for handle in &table.spill_ssts[..table.n_spill_ssts] {
            self.handles.push(handle.expect("counted SST member"));
        }
        if let Err(error) = storage.spill_roots.pin(
            &self.handles,
            self.persistence != RelationPersistence::Temporary,
        ) {
            self.handles.clear();
            return Err(error);
        }
        self.registry = Some(Arc::clone(&storage.spill_roots));
        self.reader = storage.spill.as_ref().map(Arc::clone);
        Ok(())
    }

    pub(super) fn validate(&self, storage: &Storage, table_slot: usize) -> Result<(), SqlError> {
        if self
            .registry
            .as_ref()
            .is_none_or(|registry| !Arc::ptr_eq(registry, &storage.spill_roots))
        {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "retained SST reader belongs to different storage"
            ));
        }
        if table_slot != self.table_slot
            || table_slot >= storage.physical_table_count()
            || !storage.table(table_slot).live()
            || storage.table(table_slot).created_at() != self.created_at
            || storage.table(table_slot).generation() != self.data_generation
        {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "checkpoint source table identity changed while SST roots were retained"
            ));
        }
        let table = storage.table(table_slot);
        let definition = table.definition();
        let mut schema = [ColType::Bool; MAX_COLUMNS];
        if definition.persistence != self.persistence
            || definition.schema(&mut schema) != self.n_columns
            || schema[..self.n_columns] != self.schema[..self.n_columns]
        {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "checkpoint source schema changed while SST roots were retained"
            ));
        }
        Ok(())
    }

    pub(super) fn relation(&self) -> SpillRelation<'_> {
        SpillRelation {
            persistence: self.persistence,
            schema: self.schema,
            n_columns: self.n_columns,
            handles: SpillHandles::Retained(&self.handles),
        }
    }

    pub(super) fn clear(&mut self) {
        if let Some(registry) = self.registry.take() {
            registry.release(
                &self.handles,
                self.persistence != RelationPersistence::Temporary,
            );
        }
        self.handles.clear();
        self.reader = None;
    }
}

impl Drop for SpillGenerationSnapshot {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::tests::{make_def, test_budget, test_config};
    use crate::store::{BlockId, RowSstFormat};

    fn handle(tag: u8) -> SstHandle {
        SstHandle {
            index: BlockId([tag; 32]),
            filter: BlockId([tag.wrapping_add(1); 32]),
            roster: BlockId([tag.wrapping_add(2); 32]),
            format: RowSstFormat::PackedV4,
        }
    }

    #[test]
    fn query_sst_roots_charge_exact_startup_capacity_and_reuse_released_slots() {
        let mut config = test_config();
        config.query_workspace_slots = 1;
        config.max_spill_generations_per_table = 1;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let table = storage.create_table(make_def("query_roots", &[("id", ColType::Int4, true)])).unwrap();
        storage.set_spill_list(table, &[handle(1)]);
        let expected = QueryRootPool::budget_bytes(&config);
        let mut pool_budget = Budget::new(expected);
        let pool = QueryRootPool::new(&config, &mut pool_budget).unwrap();
        assert_eq!(pool_budget.used(), expected);
        let mut leases = Vec::with_capacity(pool.slots.len());
        crate::mem::guard::forbid_alloc(|| {
            for _ in 0..pool.slots.len() {
                leases.push(pool.retain(&storage, table, 7).unwrap());
            }
            assert_eq!(pool.retain(&storage, table, 9).err().unwrap().sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            drop(leases.pop());
            let replacement = pool.retain(&storage, table, 9).unwrap();
            replacement.with_snapshot(|snapshot| {
                assert_eq!(snapshot.commit_lsn, 9);
                assert_eq!(snapshot.relation().handle(0), handle(1));
                replacement.with_snapshot(|nested| {
                    assert_eq!(nested.commit_lsn, snapshot.commit_lsn);
                    assert_eq!(nested.relation().handle(0), snapshot.relation().handle(0));
                    Ok(())
                })?;
                Ok(())
            }).unwrap();
            drop(replacement);
            leases.clear();
            assert!(pool.slots.iter().all(|slot| !slot.retained.load(Ordering::Acquire)));
        });
    }

    #[test]
    fn query_sst_roots_release_failed_capture_without_partial_pins() {
        let mut config = test_config();
        config.max_spill_generations_per_table = 2;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let table = storage.create_table(make_def("query_root_failure", &[("id", ColType::Int4, true)])).unwrap();
        storage.set_spill_list(table, &[handle(1), handle(2)]);
        config.max_spill_generations_per_table = 1;
        let mut pool_budget = Budget::new(QueryRootPool::budget_bytes(&config));
        let pool = QueryRootPool::new(&config, &mut pool_budget).unwrap();
        let mut retained = Vec::with_capacity(2);
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(pool.retain(&storage, table, 7).err().unwrap().sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(pool.slots.iter().all(|slot| !slot.retained.load(Ordering::Acquire)));
            storage.copy_retained_spill_roots(true, &mut retained).unwrap();
            assert!(retained.is_empty());
            storage.set_spill_list(table, &[handle(1)]);
            drop(pool.retain(&storage, table, 7).unwrap());
        });
    }

    #[test]
    fn query_sst_roots_read_captured_versions_after_table_and_storage_retirement() {
        use crate::sql::types::Datum;
        use crate::storage::{SpillOverlayMode, SpilledRowRepresentation, rowenc};
        use crate::storage::row_walks::RowScanIds;
        use crate::store::{EphemeralBlockStore, SstKey, SstWriter};
        let mut config = test_config();
        config.max_spill_generations_per_table = 2;
        let bytes = 32 * crate::store::BLOCK_SIZE;
        let mut budget = Budget::new(Storage::extra_budget_bytes(&config) + config.memtable_bytes
            + SpillReader::budget_bytes(false, 2) + EphemeralBlockStore::budget_bytes(bytes) + (1 << 20));
        let path = std::env::temp_dir().join(format!("pos3ql-query-root-{}.blocks", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut blocks = EphemeralBlockStore::open(&mut budget, &path, bytes).unwrap();
        let mut writer = Box::new(SstWriter::new());
        let mut row = [0u8; 32];
        for (id, lsn, value) in [(1, 9, 88), (1, 7, 41), (2, 9, 99)] {
            let values = [Datum::Int4(value)];
            let len = rowenc::encoded_len(&values);
            rowenc::encode(&values, &mut row[..len]);
            writer.append_version(&mut blocks, SstKey::at(id, lsn), &row[..len]).unwrap();
        }
        let root = writer.finish(&mut blocks).unwrap().unwrap();
        let blocks = Arc::new(Mutex::new(blocks));
        let reader = SpillReader::new(&mut budget, 2, None, Some(blocks)).unwrap();
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        storage.attach_spill(reader);
        let mut definition = make_def("retiring_query", &[("id", ColType::Int4, true)]);
        definition.persistence = RelationPersistence::Temporary;
        let table = storage.create_table(definition).unwrap();
        storage.set_spill_list(table, &[root]);
        let mut arena = crate::mem::arena::Arena::new(&mut budget, "query root result", 1 << 18).unwrap();
        let mut retained = Vec::with_capacity(1);
        crate::mem::guard::forbid_alloc(|| {
            storage.set_commit_snapshot(7);
            let scan = storage.row_scan(table).unwrap();
            let RowScanIds::Reserved(rowids) = &scan.rowids else { unreachable!() };
            let mut metadata_rows = 0;
            storage.spill_merged_walk(table, rowids, &mut |rowid, _, _, lsn| {
                assert_eq!((rowid, lsn), (1, 7));
                storage.set_commit_snapshot(u64::MAX);
                metadata_rows += 1;
                Ok(core::ops::ControlFlow::Continue(()))
            }).unwrap();
            assert_eq!(metadata_rows, 1, "nested callbacks cannot advance the outer read boundary");
            storage.set_commit_snapshot(7);
            let error = storage.spill_merged_walk(table, rowids, &mut |_, _, _, _| {
                Err(sql_err!(sqlstate::QUERY_CANCELED, "cancel retained scan"))
            }).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::QUERY_CANCELED);
            assert!(storage.query_roots.slots.iter().all(|slot| !slot.retained.load(Ordering::Acquire)));
            drop(scan);
            let lease = storage.query_roots.retain(&storage, table, 7).unwrap();
            let registry = lease.with_snapshot(|snapshot| Ok(Arc::clone(snapshot.registry.as_ref().unwrap()))).unwrap();
            storage.drop_table(table);
            storage.create_table(make_def("retiring_query", &[("id", ColType::Text, true)])).unwrap();
            drop(storage);
            registry.copy_roots(false, &mut retained).unwrap();
            assert_eq!(retained, [root]);
            let mut count = 0;
            lease.with_snapshot(|snapshot| {
                let relation = snapshot.relation();
                assert_eq!(relation.schema[0], ColType::Int4);
                assert_eq!(snapshot.commit_lsn, 7);
                Storage::walk_spill_bytes(
                    snapshot.reader.as_ref().unwrap(), &relation, snapshot.commit_lsn,
                    &arena, true, None, true,
                    SpillOverlayMode::VisibleScan(&RowScanIds::Statement(&[])), None,
                    &mut |rowid, lsn, representation| {
                        assert_eq!((rowid, lsn), (1, 7));
                        let mut decoded = [Datum::Null; MAX_COLUMNS];
                        let values = match representation {
                            SpilledRowRepresentation::Encoded(bytes) => {
                                rowenc::decode(bytes, &relation.schema[..relation.n_columns], &mut decoded)?;
                                &decoded[..relation.n_columns]
                            }
                            SpilledRowRepresentation::Values(values) => values,
                        };
                        assert_eq!(values, [Datum::Int4(41)]);
                        count += 1;
                        Ok(core::ops::ControlFlow::Continue(()))
                    },
                )?;
                Ok(())
            }).unwrap();
            assert_eq!(count, 1);
            drop(lease);
            registry.copy_roots(false, &mut retained).unwrap();
            assert!(retained.is_empty());
            assert!(registry.take_reclamation_pending());
        });
        arena.reset();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn retained_sst_registry_preflights_capacity_and_reader_overflow() {
        let mut config = test_config();
        config.max_tables = 1;
        config.max_spill_generations_per_table = 1;
        let mut budget = Budget::new(SpillRootRegistry::budget_bytes(&config));
        let registry = SpillRootRegistry::new(&config, &mut budget).unwrap();
        assert_eq!(budget.remaining(), 0);
        let roots = [handle(1), handle(5), handle(9), handle(13)];
        let mut copied = Vec::with_capacity(4);
        crate::mem::guard::forbid_alloc(|| {
            registry.pin(&roots, true).unwrap();
            registry.pin(&[roots[0], roots[0]], true).unwrap();
            let error = registry.pin(&[handle(17)], true).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            registry.copy_roots(true, &mut copied).unwrap();
            assert_eq!(copied, roots);
            {
                let mut pinned = registry.roots.lock().unwrap();
                assert_eq!(pinned[0].readers.get(), 3);
                pinned[0].readers = NonZeroUsize::new(usize::MAX).unwrap();
            }
            let error = registry.pin(&roots[..2], true).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            {
                let mut pinned = registry.roots.lock().unwrap();
                assert_eq!(
                    pinned[1].readers.get(),
                    1,
                    "failure cannot partially pin another root"
                );
                pinned[0].readers = NonZeroUsize::new(3).unwrap();
            }
            registry.release(&[roots[0], roots[0]], true);
            registry.copy_roots(true, &mut copied).unwrap();
            assert_eq!(copied.len(), 4);
            registry.release(&roots, true);
            assert!(registry.take_reclamation_pending());
            assert!(!registry.take_reclamation_pending());
            registry.copy_roots(true, &mut copied).unwrap();
            assert!(copied.is_empty());
            registry.pin(&[handle(17)], true).unwrap();
            registry.release(&[handle(17)], true);
        });
    }

    #[test]
    fn retained_sst_registry_keeps_durable_and_temporary_namespaces_separate() {
        let config = test_config();
        let mut budget = Budget::new(SpillRootRegistry::budget_bytes(&config));
        let registry = SpillRootRegistry::new(&config, &mut budget).unwrap();
        let root = handle(1);
        let mut copied = Vec::with_capacity(1);
        let mut too_small = Vec::new();
        crate::mem::guard::forbid_alloc(|| {
            registry.pin(&[root], true).unwrap();
            registry.pin(&[root], false).unwrap();
            let error = registry.copy_roots(true, &mut too_small).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            registry.copy_roots(true, &mut copied).unwrap();
            assert_eq!(copied, [root]);
            registry.copy_roots(false, &mut copied).unwrap();
            assert_eq!(copied, [root]);
            registry.release(&[root], true);
            registry.copy_roots(true, &mut copied).unwrap();
            assert!(copied.is_empty());
            registry.copy_roots(false, &mut copied).unwrap();
            assert_eq!(copied, [root]);
            registry.release(&[root], false);
            registry.copy_roots(false, &mut copied).unwrap();
            assert!(copied.is_empty());
        });
    }

    #[test]
    fn retained_sst_images_survive_replacement_and_storage_drop() {
        let mut config = test_config();
        config.max_spill_generations_per_table = 1;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let table = storage
            .create_table(make_def("retained_sst", &[("id", ColType::Int4, true)]))
            .unwrap();
        let original = handle(1);
        let replacement = handle(5);
        storage.set_spill_list(table, &[original]);
        let mut snapshot = SpillGenerationSnapshot::new(1);
        let mut too_small = SpillGenerationSnapshot::new(0);
        let mut roots = Vec::with_capacity(1);
        let mut other_budget = test_budget(&config);
        let other = Storage::new(&config, &mut other_budget).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let error = too_small.capture(&storage, table).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(too_small.registry.is_none());
            snapshot.capture(&storage, table).unwrap();
            assert!(snapshot.validate(&storage, table).is_ok());
            assert_eq!(
                snapshot.validate(&other, table).unwrap_err().sqlstate,
                sqlstate::INTERNAL_ERROR
            );
            storage.collapse_spill(table, replacement);
            assert_eq!(snapshot.relation().handle(0), original);
            assert_eq!(
                SpillRelation::installed(storage.table(table)).handle(0),
                replacement
            );
            storage.drop_table(table);
            assert_eq!(
                snapshot.validate(&storage, table).unwrap_err().sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            let registry = Arc::clone(snapshot.registry.as_ref().unwrap());
            drop(storage);
            registry.copy_roots(true, &mut roots).unwrap();
            assert_eq!(roots, [original]);
            assert_eq!(snapshot.relation().schema[0], ColType::Int4);
            snapshot.clear();
            registry.copy_roots(true, &mut roots).unwrap();
            assert!(roots.is_empty());
        });
    }
}
