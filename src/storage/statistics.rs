//! Statistics visibility and saved-image acknowledgements have separate ownership.

use core::mem::size_of;
use core::num::NonZeroU64;
use {
    crate::mem::mutex::Mutex, std::sync::Arc, std::sync::RwLock, std::sync::RwLockReadGuard,
    std::sync::RwLockWriteGuard,
};

use super::{
    CatalogPosition, PendingTableStatisticsSlot, RelationPersistence, SqlName, Storage,
    TableStatistics, pending_table_statistics_capacity, table_slot_capacity,
};
use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

#[derive(Clone, Copy)]
struct PendingStatisticsHead {
    transaction: u32,
    tail: usize,
}

struct StatisticsState {
    committed: TableStatistics,
    pending: Option<PendingStatisticsHead>,
    dirty: bool,
    wal_dirty: bool,
    relation_revision: u64,
    has_index_at: Option<NonZeroU64>,
}

pub(super) struct TableStatisticsOwner {
    state: Arc<RwLock<StatisticsState>>,
}

impl TableStatisticsOwner {
    pub(super) const fn allocation_bytes() -> usize {
        size_of::<RwLock<StatisticsState>>() + 2 * size_of::<usize>()
    }

    pub(super) fn new(budget: &mut Budget) -> Result<Self, BudgetError> {
        budget.draw(Self::allocation_bytes(), "table statistics ownership")?;
        Ok(Self {
            state: Arc::new(RwLock::new(StatisticsState {
                committed: TableStatistics::EMPTY,
                pending: None,
                dirty: false,
                wal_dirty: false,
                relation_revision: 0,
                has_index_at: None,
            })),
        })
    }

    fn read(&self) -> RwLockReadGuard<'_, StatisticsState> {
        self.state.read().expect("table statistics lock poisoned")
    }

    fn write(&self) -> RwLockWriteGuard<'_, StatisticsState> {
        self.state.write().expect("table statistics lock poisoned")
    }

    pub(super) fn set_image(&self, image: TableStatistics, dirty: Option<bool>, clear_wal: bool) {
        let mut state = self.write();
        state.committed = image;
        if let Some(dirty) = dirty {
            state.dirty = dirty;
        }
        if clear_wal {
            state.wal_dirty = false;
        }
        // Reusing a table slot must not reuse an acknowledgement identity.
        // New ANALYZE publication advances this revision; resets preserve it.
    }

    pub(super) fn clear_wal(&self) {
        self.write().wal_dirty = false;
    }

    pub(super) fn has_index(&self, created_at: u64) -> bool {
        self.read()
            .has_index_at
            .is_some_and(|identity| identity.get() == created_at)
    }

    pub(super) fn set_index_hint(
        &self,
        created_at: u64,
        has_index: bool,
        dirty: bool,
    ) -> Result<(), SqlError> {
        let identity = NonZeroU64::new(created_at).ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "relation index hint requires a creation identity"
            )
        })?;
        let mut state = self.write();
        let next = has_index.then_some(identity);
        if state.has_index_at == next {
            return Ok(());
        }
        let revision = state.relation_revision.checked_add(1).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "table statistics relation revision is exhausted"
            )
        })?;
        state.has_index_at = next;
        state.relation_revision = revision;
        state.dirty |= dirty;
        state.wal_dirty |= dirty;
        Ok(())
    }
}

pub(crate) struct CheckpointStatistics {
    table: usize,
    created_at: u64,
    state: Arc<RwLock<StatisticsState>>,
    image: TableStatistics,
    revision: u64,
    has_index_at: Option<NonZeroU64>,
}

impl CheckpointStatistics {
    pub(crate) fn image(&self) -> TableStatistics {
        self.image
    }

    pub(crate) fn has_index(&self) -> bool {
        self.has_index_at
            .is_some_and(|identity| identity.get() == self.created_at)
    }

    pub(crate) fn acknowledge(self, storage: &Storage) {
        let table = &storage.tables[self.table];
        assert!(
            Arc::ptr_eq(&self.state, &table.statistics.state),
            "checkpoint statistics belong to another storage owner"
        );
        let definition = table.definition.read();
        if definition.identity.created_at != self.created_at || !definition.identity.live() {
            return;
        }
        let mut state = self.state.write().expect("table statistics lock poisoned");
        if state.relation_revision == self.revision
            && state.committed == self.image
            && state.has_index_at == self.has_index_at
        {
            state.dirty = false;
            if state.pending.is_none() {
                state.wal_dirty = false;
            }
        }
    }
}

pub(super) struct StatisticsVersions {
    state: RwLock<FixedVec<PendingTableStatisticsSlot>>,
}

impl StatisticsVersions {
    pub(super) fn budget_bytes(config: &Config) -> usize {
        size_of::<Self>()
            + pending_table_statistics_capacity(config) * size_of::<PendingTableStatisticsSlot>()
    }

    pub(super) fn new(config: &Config, budget: &mut Budget) -> Result<Self, BudgetError> {
        budget.draw(size_of::<Self>(), "table statistics version controls")?;
        Ok(Self {
            state: RwLock::new(FixedVec::new(
                budget,
                "pending_table_statistics",
                pending_table_statistics_capacity(config),
            )?),
        })
    }

    #[cfg(test)]
    pub(super) fn capacity(&self) -> usize {
        self.state.read().unwrap().capacity()
    }
}

/// Column statistics follow transaction visibility. This receipt acknowledges
/// only the in-place relation counts captured for the journal record.
struct RelationStatisticsReceipt {
    state: Arc<RwLock<StatisticsState>>,
    revision: u64,
}

impl RelationStatisticsReceipt {
    fn acknowledge(self) {
        let mut state = self.state.write().expect("table statistics lock poisoned");
        if state.relation_revision == self.revision {
            state.wal_dirty = false;
        }
    }
}

pub(crate) struct StatisticsWalImage {
    pub(crate) position: CatalogPosition,
    pub(crate) has_index: bool,
    pub(crate) schema: SqlName,
    pub(crate) name: SqlName,
    pub(crate) persistence: RelationPersistence,
    pub(crate) statistics: TableStatistics,
    receipt: RelationStatisticsReceipt,
}

struct ReceiptWorkspace {
    retained: bool,
    receipts: FixedVec<RelationStatisticsReceipt>,
}

pub(crate) struct StatisticsReceiptPool {
    slots: FixedVec<Mutex<ReceiptWorkspace>>,
}

pub(crate) struct StatisticsReceiptLease {
    pool: Arc<StatisticsReceiptPool>,
    slot: usize,
}

impl StatisticsReceiptPool {
    pub(crate) fn budget_bytes(config: &Config) -> usize {
        size_of::<Self>()
            + 2 * size_of::<usize>()
            + config.query_workspace_slots
                * (size_of::<Mutex<ReceiptWorkspace>>()
                    + table_slot_capacity(config) * size_of::<RelationStatisticsReceipt>())
    }

    pub(crate) fn new(config: &Config, budget: &mut Budget) -> Result<Arc<Self>, BudgetError> {
        budget.draw(
            size_of::<Self>() + 2 * size_of::<usize>(),
            "statistics receipt pool controls",
        )?;
        let mut slots = FixedVec::new(
            budget,
            "statistics receipt workspaces",
            config.query_workspace_slots,
        )?;
        for _ in 0..config.query_workspace_slots {
            slots
                .push(Mutex::new(ReceiptWorkspace {
                    retained: false,
                    receipts: FixedVec::new(
                        budget,
                        "statistics journal receipts",
                        table_slot_capacity(config),
                    )?,
                }))
                .expect("sized to query_workspace_slots");
        }
        Ok(Arc::new(Self { slots }))
    }

    pub(crate) fn retain(self: &Arc<Self>) -> Result<StatisticsReceiptLease, SqlError> {
        self.retain_at(crate::sql::execution_query_workspace().index())
    }

    fn retain_at(self: &Arc<Self>, slot: usize) -> Result<StatisticsReceiptLease, SqlError> {
        let workspace = self.slots.get(slot).ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "statistics receipt workspace is outside its startup reservation"
            )
        })?;
        let mut workspace = workspace.lock().expect("statistics receipt lock poisoned");
        if workspace.retained {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "statistics receipt workspace is already retained"
            ));
        }
        workspace.retained = true;
        Ok(StatisticsReceiptLease {
            pool: Arc::clone(self),
            slot,
        })
    }
}

impl StatisticsReceiptLease {
    pub(crate) fn retain_image(&self, image: &StatisticsWalImage) -> Result<(), SqlError> {
        // Share the captured receipt; do not recapture the live statistics cell.
        let receipt = RelationStatisticsReceipt {
            state: Arc::clone(&image.receipt.state),
            revision: image.receipt.revision,
        };
        self.pool.slots[self.slot]
            .lock()
            .expect("statistics receipt lock poisoned")
            .receipts
            .push(receipt)
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "statistics journal receipts exceed the table reservation"
                )
            })
    }

    pub(crate) fn acknowledge(&self) {
        loop {
            let receipt = self.pool.slots[self.slot]
                .lock()
                .expect("statistics receipt lock poisoned")
                .receipts
                .pop();
            let Some(receipt) = receipt else { break };
            receipt.acknowledge();
        }
    }
}

impl Drop for StatisticsReceiptLease {
    fn drop(&mut self) {
        let mut workspace = self.pool.slots[self.slot]
            .lock()
            .expect("statistics receipt lock poisoned");
        workspace.receipts.clear();
        workspace.retained = false;
    }
}

fn clear_pending(state: &mut StatisticsState, versions: &mut FixedVec<PendingTableStatisticsSlot>) {
    let mut tail = state.pending.take().map(|head| head.tail);
    while let Some(slot) = tail {
        let entry = &mut versions[slot];
        tail = entry.previous;
        entry.used = false;
    }
}

impl Storage {
    pub(crate) fn table_statistics(&self, table_slot: usize, txid: u32) -> TableStatistics {
        let state = self.tables[table_slot].statistics.read();
        let mut image = state.committed;
        if let Some(head) = state.pending.filter(|head| head.transaction == txid) {
            image.columns = self
                .pending_table_statistics
                .state
                .read()
                .expect("statistics versions lock poisoned")[head.tail]
                .columns;
        }
        image
    }

    #[cfg(test)]
    pub(crate) fn pending_table_statistics(
        &self,
        table_slot: usize,
        txid: u32,
    ) -> Option<[super::ColumnStatistics; super::MAX_COLUMNS]> {
        let state = self.tables[table_slot].statistics.read();
        state
            .pending
            .filter(|head| head.transaction == txid)
            .map(|head| {
                self.pending_table_statistics
                    .state
                    .read()
                    .expect("statistics versions lock poisoned")[head.tail]
                    .columns
            })
    }

    pub(crate) fn capture_statistics_for_wal(
        &self,
        table_slot: usize,
        txid: u32,
    ) -> Option<StatisticsWalImage> {
        let definition = self.table_def(table_slot, txid);
        if definition.identity().database != super::current_database()
            || !definition.identity().existence.visible_to(txid)
        {
            return None;
        }
        let state = self.tables[table_slot].statistics.read();
        let pending = state.pending.filter(|head| head.transaction == txid);
        if pending.is_none() && !state.wal_dirty {
            return None;
        }
        let mut statistics = state.committed;
        if let Some(head) = pending {
            statistics.columns = self
                .pending_table_statistics
                .state
                .read()
                .expect("statistics versions lock poisoned")[head.tail]
                .columns;
        }
        Some(StatisticsWalImage {
            position: CatalogPosition::new(table_slot, definition.identity().created_at)
                .expect("a visible user table has a durable position"),
            has_index: state
                .has_index_at
                .is_some_and(|identity| identity.get() == definition.identity().created_at),
            schema: definition.schema,
            name: definition.name,
            persistence: definition.persistence,
            statistics,
            receipt: RelationStatisticsReceipt {
                state: Arc::clone(&self.tables[table_slot].statistics.state),
                revision: state.relation_revision,
            },
        })
    }

    pub(super) fn write_table_statistics(
        &self,
        table_slot: usize,
        txid: u32,
        statistics: TableStatistics,
    ) -> Result<(), SqlError> {
        let name = self.tables[table_slot].definition().name;
        let mut state = self.tables[table_slot].statistics.write();
        if state.pending.is_some_and(|head| head.transaction != txid) {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "could not serialize ANALYZE of relation \"{}\"",
                name.as_str()
            ));
        }
        let revision = state.relation_revision.checked_add(1).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "table statistics relation revision is exhausted"
            )
        })?;
        let mut versions = self
            .pending_table_statistics
            .state
            .write()
            .expect("statistics versions lock poisoned");
        let previous = state.pending.map(|head| head.tail);
        if previous.is_some_and(|slot| versions[slot].depth >= self.max_catalog_versions_per_object)
        {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "relation \"{}\" exceeds max_catalog_versions_per_object ({})",
                name.as_str(),
                self.max_catalog_versions_per_object
            ));
        }
        let depth = previous.map_or(1, |slot| versions[slot].depth + 1);
        let entry = PendingTableStatisticsSlot {
            used: true,
            previous,
            depth,
            columns: statistics.columns,
        };
        let slot = if let Some(slot) = versions.iter().position(|entry| !entry.used) {
            versions[slot] = entry;
            slot
        } else {
            let slot = versions.len();
            versions.push(entry).map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "pending table-statistics pool is exhausted"
                )
            })?;
            slot
        };
        state.pending = Some(PendingStatisticsHead {
            transaction: txid,
            tail: slot,
        });
        // PostgreSQL keeps relation counts after rollback; columns stay private.
        state.committed.valid = statistics.valid;
        state.committed.rows = statistics.rows;
        state.committed.average_row_width = statistics.average_row_width;
        state.committed.analyzed_generation = statistics.analyzed_generation;
        state.relation_revision = revision;
        state.dirty = true;
        state.wal_dirty = true;
        Ok(())
    }

    /// Relation estimates change in place; column statistics keep their owner.
    pub(crate) fn write_relation_counts(
        &self,
        table_slot: usize,
        statistics: TableStatistics,
        dirty_wal: bool,
    ) -> Result<(), SqlError> {
        let mut state = self.tables[table_slot].statistics.write();
        let revision = state.relation_revision.checked_add(1).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "table statistics relation revision is exhausted"
            )
        })?;
        state.committed.valid = statistics.valid;
        state.committed.rows = statistics.rows;
        state.committed.average_row_width = statistics.average_row_width;
        state.committed.analyzed_generation = statistics.analyzed_generation;
        state.relation_revision = revision;
        state.dirty = true;
        state.wal_dirty |= dirty_wal;
        Ok(())
    }

    pub(crate) fn rollback_table_statistics(&self, table_slot: usize, txid: u32) {
        let mut state = self.tables[table_slot].statistics.write();
        let Some(head) = state.pending.filter(|head| head.transaction == txid) else {
            return;
        };
        let mut versions = self
            .pending_table_statistics
            .state
            .write()
            .expect("statistics versions lock poisoned");
        let previous = versions[head.tail].previous;
        versions[head.tail].used = false;
        state.pending = previous.map(|tail| PendingStatisticsHead {
            transaction: txid,
            tail,
        });
    }

    pub(super) fn clear_pending_table_statistics(&self, table_slot: usize) {
        let mut state = self.tables[table_slot].statistics.write();
        let mut versions = self
            .pending_table_statistics
            .state
            .write()
            .expect("statistics versions lock poisoned");
        clear_pending(&mut state, &mut versions);
    }

    pub(crate) fn commit_table_statistics(&self, table_slot: usize, txid: u32) {
        let mut state = self.tables[table_slot].statistics.write();
        let Some(head) = state.pending.filter(|head| head.transaction == txid) else {
            return;
        };
        let mut versions = self
            .pending_table_statistics
            .state
            .write()
            .expect("statistics versions lock poisoned");
        state.committed.columns = versions[head.tail].columns;
        state.dirty = true;
        clear_pending(&mut state, &mut versions);
    }

    /// Estimates consult metadata and resident counts without object I/O.
    pub(crate) fn planning_row_estimate(&self, table_slot: usize) -> u64 {
        let table = &self.tables[table_slot];
        let estimate = {
            let state = table.statistics.read();
            state.committed.valid.then_some(state.committed.rows)
        };
        if let Some(rows) = estimate {
            rows
        } else if table.spill_generations().is_empty() {
            table.rows.len() as u64
        } else {
            (table.rows.len() as u64).max(1_000)
        }
    }

    pub(crate) fn capture_checkpoint_statistics(&self, table_slot: usize) -> CheckpointStatistics {
        let table = &self.tables[table_slot];
        let definition = table.definition.read();
        let state = table.statistics.read();
        CheckpointStatistics {
            table: table_slot,
            created_at: definition.identity.created_at,
            state: Arc::clone(&table.statistics.state),
            image: state.committed,
            revision: state.relation_revision,
            has_index_at: state.has_index_at,
        }
    }

    pub(crate) fn relation_has_index(&self, position: CatalogPosition) -> bool {
        self.tables[position.slot()]
            .statistics
            .has_index(position.created_at())
    }

    pub(crate) fn vacuum_relation_index_hint(&self, table_slot: usize) -> Result<(), SqlError> {
        let table = &self.tables[table_slot];
        let (definition, indexes) = super::row_ownership::acquire(|| {
            Ok((
                table.definition.try_read_for_batch()?,
                super::row_ownership::try_mutex(
                    &self.index_catalog,
                    "index catalog lock poisoned",
                )?,
            ))
        });
        let has_index = definition
            .committed()
            .columns()
            .iter()
            .any(|column| column.unique)
            || definition.committed().n_uniques != 0
            || indexes.iter().any(|index| {
                index.ddl_state == super::CatalogDdlState::Present
                    && index.table_binding.is_some_and(|binding| {
                        binding.source.slot()
                            == super::catalog_sources::CatalogObjectSlot::Table(table_slot)
                            && binding.source.matches_lifetime(
                                self,
                                definition.identity.database,
                                definition.identity.created_at,
                                definition.identity.existence,
                            )
                    })
            });
        table
            .statistics
            .set_index_hint(definition.identity.created_at, has_index, true)
    }

    pub(crate) fn replay_relation_index_hint(
        &self,
        position: CatalogPosition,
        has_index: bool,
        transaction: u32,
    ) -> Result<(), SqlError> {
        let table = self.tables.get(position.slot()).ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "relation index hint references an unknown table"
            )
        })?;
        let definition = table.definition.read();
        if definition.identity.created_at != position.created_at()
            || definition.identity.database != super::current_database()
            || !definition.identity.existence.visible_to(transaction)
        {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "relation index hint references a different table lifetime"
            ));
        }
        table
            .statistics
            .set_index_hint(position.created_at(), has_index, false)
    }

    pub(crate) fn statistics_dirty(&self) -> bool {
        self.tables
            .iter()
            .any(|table| table.live() && table.statistics.read().dirty)
    }

    #[cfg(test)]
    pub(crate) fn statistics_wal_dirty(&self, table_slot: usize) -> bool {
        self.tables[table_slot].statistics.read().wal_dirty
    }

    fn replace_table_statistics(
        &self,
        table_slot: usize,
        statistics: TableStatistics,
        dirty: bool,
    ) {
        let mut state = self.tables[table_slot].statistics.write();
        let mut versions = self
            .pending_table_statistics
            .state
            .write()
            .expect("statistics versions lock poisoned");
        clear_pending(&mut state, &mut versions);
        state.committed = statistics;
        state.dirty = dirty;
        state.wal_dirty = false;
    }

    pub(crate) fn install_table_statistics(&self, table_slot: usize, statistics: TableStatistics) {
        self.replace_table_statistics(table_slot, statistics, false);
    }

    pub(crate) fn replay_table_statistics(&self, table_slot: usize, statistics: TableStatistics) {
        self.replace_table_statistics(table_slot, statistics, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{ColType, ColumnStatistics, DatabaseOid};

    #[test]
    fn catalog_snapshot_relation_index_hints_preserve_receipts_and_table_lifetimes() {
        let (mut storage, mut budget, table) = fixture();
        let config = super::super::tests::test_config();
        let receipts = StatisticsReceiptPool::new(&config, &mut budget).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let position = storage.table_position(table);
            storage.tables[table]
                .statistics
                .set_index_hint(position.created_at(), true, true)
                .unwrap();
            let captured = storage.capture_statistics_for_wal(table, 7).unwrap();
            assert!(captured.has_index);
            assert!(!captured.statistics.valid);
            let receipt = receipts.retain().unwrap();
            receipt.retain_image(&captured).unwrap();
            storage.vacuum_relation_index_hint(table).unwrap();
            receipt.acknowledge();
            assert!(!storage.relation_has_index(position));
            assert!(storage.statistics_wal_dirty(table));
            storage.tables[table]
                .statistics
                .set_index_hint(position.created_at(), true, true)
                .unwrap();
            let checkpoint = storage.capture_checkpoint_statistics(table);
            storage.vacuum_relation_index_hint(table).unwrap();
            checkpoint.acknowledge(&storage);
            assert!(storage.statistics_dirty());
            storage.tables[table]
                .statistics
                .set_index_hint(position.created_at(), true, true)
                .unwrap();
            storage.commit_drop(table);
            let replacement = storage
                .create_table(super::super::tests::make_def(
                    "new_index_hint_lifetime",
                    &[("id", ColType::Int4, true)],
                ))
                .unwrap();
            assert_eq!(replacement, table);
            assert!(!storage.relation_has_index(storage.table_position(replacement)));
            assert!(
                storage
                    .replay_relation_index_hint(position, true, 0)
                    .is_err()
            );
            let current = storage.table_position(replacement);
            storage
                .replay_relation_index_hint(current, true, 0)
                .unwrap();
            assert!(storage.relation_has_index(current));
            storage.tables[table].statistics.write().relation_revision = u64::MAX;
            let error = storage.tables[table]
                .statistics
                .set_index_hint(current.created_at(), false, true)
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(
                error.message.as_str(),
                "table statistics relation revision is exhausted"
            );
            assert!(storage.relation_has_index(current));
        });
    }

    fn image(rows: u64) -> TableStatistics {
        let mut image = TableStatistics::EMPTY;
        image.valid = true;
        image.rows = rows;
        image.average_row_width = (rows * 2) as u32;
        image.analyzed_generation = rows * 3;
        image.columns[0] = ColumnStatistics {
            valid: true,
            distinct_values: rows * 5,
            ..ColumnStatistics::EMPTY
        };
        image
    }

    fn fixture() -> (Storage, Budget, usize) {
        let config = super::super::tests::test_config();
        let mut budget = super::super::tests::test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let table = storage
            .create_table(super::super::tests::make_def(
                "statistics_owner",
                &[("id", ColType::Int4, true)],
            ))
            .unwrap();
        (storage, budget, table)
    }

    #[test]
    fn statistics_ownership_old_acknowledgements_preserve_newer_and_reused_images() {
        let (mut storage, mut budget, table) = fixture();
        let config = super::super::tests::test_config();
        let pool = StatisticsReceiptPool::new(&config, &mut budget).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            storage.write_table_statistics(table, 7, image(1)).unwrap();
            let captured = storage.capture_statistics_for_wal(table, 7).unwrap();
            let receipt = pool.retain().unwrap();
            receipt.retain_image(&captured).unwrap();
            storage.write_table_statistics(table, 7, image(2)).unwrap();
            receipt.acknowledge();
            assert!(storage.statistics_wal_dirty(table));
            assert_eq!(storage.table_statistics(table, 7), image(2));
            let latest = storage.capture_statistics_for_wal(table, 7).unwrap();
            receipt.retain_image(&latest).unwrap();
            receipt.acknowledge();
            assert!(!storage.statistics_wal_dirty(table));
            storage.rollback_table_statistics(table, 7);
            storage.rollback_table_statistics(table, 7);
            storage.commit_drop(table);
            let replacement = storage
                .create_table(super::super::tests::make_def(
                    "replacement_statistics",
                    &[("id", ColType::Int4, true)],
                ))
                .unwrap();
            assert_eq!(replacement, table);
            storage.write_table_statistics(table, 8, image(3)).unwrap();
            receipt.retain_image(&latest).unwrap();
            receipt.acknowledge();
            assert!(storage.statistics_wal_dirty(table));
            assert_eq!(storage.table_statistics(table, 8), image(3));
        });
    }

    #[test]
    fn statistics_ownership_copied_readers_observe_postgresql_visibility_during_updates() {
        let (storage, _, table) = fixture();
        let before = storage.table_statistics(table, 0);
        let start = std::sync::Barrier::new(3);
        std::thread::scope(|scope| {
            let storage_ref = &storage;
            let start_ref = &start;
            let writer = scope.spawn(move || {
                start_ref.wait();
                crate::mem::guard::forbid_alloc(|| {
                    for rows in 1..=100 {
                        storage_ref
                            .write_table_statistics(table, 7, image(rows))
                            .unwrap();
                        storage_ref.commit_table_statistics(table, 7);
                    }
                });
            });
            let storage_ref = &storage;
            let start_ref = &start;
            let reader = scope.spawn(move || {
                start_ref.wait();
                crate::mem::guard::forbid_alloc(|| {
                    for _ in 0..500 {
                        let own = storage_ref.table_statistics(table, 7);
                        if own.valid {
                            assert_eq!(own, image(own.rows));
                        }
                        let other = storage_ref.table_statistics(table, 0);
                        if other.valid {
                            assert_eq!(other.average_row_width as u64, other.rows * 2);
                            assert_eq!(other.analyzed_generation, other.rows * 3);
                            let prior_columns = other.columns[0].distinct_values / 5;
                            assert!(prior_columns == other.rows || prior_columns + 1 == other.rows);
                        }
                    }
                });
            });
            start.wait();
            crate::mem::guard::forbid_alloc(|| {
                for _ in 0..100 {
                    storage.heap.append_bytes(b"row!").unwrap();
                }
                assert_eq!(before, TableStatistics::EMPTY);
            });
            writer.join().unwrap();
            reader.join().unwrap();
        });
        assert_eq!(storage.table_statistics(table, 0), image(100));
    }

    #[test]
    fn statistics_ownership_pending_versions_rollback_and_exhaustion_preserve_counts() {
        let mut config = super::super::tests::test_config();
        config.max_catalog_versions_per_object = 2;
        let mut budget = super::super::tests::test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let table = storage
            .create_table(super::super::tests::make_def(
                "statistics_limits",
                &[("id", ColType::Int4, true)],
            ))
            .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            storage.write_table_statistics(table, 7, image(1)).unwrap();
            storage.write_table_statistics(table, 7, image(2)).unwrap();
            assert_eq!(
                storage
                    .write_table_statistics(table, 7, image(3))
                    .unwrap_err()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(
                storage
                    .write_table_statistics(table, 8, image(3))
                    .unwrap_err()
                    .sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            assert_eq!(storage.table_statistics(table, 7), image(2));
            storage.rollback_table_statistics(table, 7);
            let restored = storage.table_statistics(table, 7);
            assert_eq!(restored.rows, 2);
            assert_eq!(restored.average_row_width, 4);
            assert_eq!(restored.analyzed_generation, 6);
            assert_eq!(restored.columns, image(1).columns);
            storage.rollback_table_statistics(table, 7);
            let committed = storage.table_statistics(table, 0);
            assert_eq!(committed.rows, 2);
            assert!(!committed.columns[0].valid);
            assert!(storage.statistics_wal_dirty(table));
            let slots = storage.pending_table_statistics.state.read().unwrap().len();
            storage.write_table_statistics(table, 8, image(4)).unwrap();
            storage.commit_table_statistics(table, 8);
            assert_eq!(
                storage.pending_table_statistics.state.read().unwrap().len(),
                slots
            );
            assert_eq!(storage.table_statistics(table, 0), image(4));
            storage.tables[table].statistics.write().relation_revision = u64::MAX;
            assert_eq!(
                storage
                    .write_table_statistics(table, 9, image(5))
                    .unwrap_err()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(storage.table_statistics(table, 0), image(4));
            assert!(storage.pending_table_statistics(table, 9).is_none());
        });
    }

    #[test]
    fn statistics_ownership_receipt_workspaces_are_bounded_and_release_after_errors() {
        let (storage, _, table) = fixture();
        let mut config = super::super::tests::test_config();
        config.max_tables = 1;
        config.query_workspace_slots = 2;
        let mut budget = Budget::new(StatisticsReceiptPool::budget_bytes(&config));
        let pool = StatisticsReceiptPool::new(&config, &mut budget).unwrap();
        assert_eq!(budget.remaining(), 0);
        crate::mem::guard::forbid_alloc(|| {
            storage.write_table_statistics(table, 7, image(1)).unwrap();
            let captured = storage.capture_statistics_for_wal(table, 7).unwrap();
            let first = pool.retain_at(0).unwrap();
            let second = pool.retain_at(1).unwrap();
            assert_eq!(
                pool.retain_at(0).err().unwrap().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(
                pool.retain_at(2).err().unwrap().sqlstate,
                sqlstate::INTERNAL_ERROR
            );
            for _ in 0..table_slot_capacity(&config) {
                first.retain_image(&captured).unwrap();
            }
            assert_eq!(
                first.retain_image(&captured).unwrap_err().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(
                pool.slots[0].lock().unwrap().receipts.len(),
                table_slot_capacity(&config)
            );
            drop(first);
            assert!(storage.statistics_wal_dirty(table));
            let reused = pool.retain_at(0).unwrap();
            assert!(pool.slots[0].lock().unwrap().receipts.is_empty());
            second.retain_image(&captured).unwrap();
            second.acknowledge();
            assert!(!storage.statistics_wal_dirty(table));
            drop(reused);
            drop(second);
        });
    }

    #[test]
    fn statistics_ownership_retained_acknowledgements_do_not_touch_another_storage_owner() {
        let (storage, mut budget, table) = fixture();
        let config = super::super::tests::test_config();
        let pool = StatisticsReceiptPool::new(&config, &mut budget).unwrap();
        let saved = crate::mem::guard::forbid_alloc(|| {
            storage.write_table_statistics(table, 7, image(1)).unwrap();
            let captured = storage.capture_statistics_for_wal(table, 7).unwrap();
            let saved = pool.retain().unwrap();
            saved.retain_image(&captured).unwrap();
            drop(storage);
            saved
        });
        let (replacement, _, replacement_table) = fixture();
        crate::mem::guard::forbid_alloc(|| {
            replacement
                .write_table_statistics(replacement_table, 7, image(99))
                .unwrap();
            drop(pool);
            saved.acknowledge();
            assert!(replacement.statistics_wal_dirty(replacement_table));
            assert_eq!(
                replacement.table_statistics(replacement_table, 7),
                image(99)
            );
            drop(saved);
        });
    }

    #[test]
    fn statistics_ownership_journaling_keeps_other_databases_pending() {
        let (mut storage, _, table) = fixture();
        let foreign = DatabaseOid::parse(16_385).unwrap();
        storage.tables[table].identity_mut().database = foreign;
        crate::mem::guard::forbid_alloc(|| {
            storage.write_table_statistics(table, 7, image(1)).unwrap();
            storage.rollback_table_statistics(table, 7);
            assert!(storage.capture_statistics_for_wal(table, 8).is_none());
            assert!(storage.statistics_wal_dirty(table));
            crate::sql::set_execution_database_oid(foreign);
            let captured = storage.capture_statistics_for_wal(table, 8).unwrap();
            assert_eq!(captured.statistics.rows, 1);
            captured.receipt.acknowledge();
            assert!(!storage.statistics_wal_dirty(table));
            crate::sql::set_execution_database_oid(DatabaseOid::POSTGRES);
        });
    }

    #[test]
    fn parallel_physical_manifest_statistics_ack_preserves_later_and_pending_analysis() {
        let (storage, _budget, table) = fixture();
        crate::mem::guard::forbid_alloc(|| {
            storage.write_table_statistics(table, 7, image(1)).unwrap();
            storage.commit_table_statistics(table, 7);
            let old = storage.capture_checkpoint_statistics(table);
            storage.write_table_statistics(table, 9, image(2)).unwrap();
            old.acknowledge(&storage);
            assert!(storage.statistics_dirty());
            assert!(storage.statistics_wal_dirty(table));
            storage.commit_table_statistics(table, 9);

            let mut same_counts = image(2);
            same_counts.columns[0].distinct_values = 999;
            storage
                .write_table_statistics(table, 11, same_counts)
                .unwrap();
            let pending = storage.capture_checkpoint_statistics(table);
            pending.acknowledge(&storage);
            assert!(
                storage.statistics_wal_dirty(table),
                "a saved committed image cannot acknowledge private column statistics"
            );
            storage.commit_table_statistics(table, 11);
            assert!(storage.statistics_dirty());
            assert_eq!(storage.table_statistics(table, 0), same_counts);

            let current = storage.capture_checkpoint_statistics(table);
            current.acknowledge(&storage);
            assert!(!storage.statistics_dirty());
            assert!(!storage.statistics_wal_dirty(table));
        });
    }
}
