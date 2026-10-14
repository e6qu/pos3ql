//! Startup-bounded row identity retention for authoritative metadata walks.

use core::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard};

use super::row_images::RowVersionIdentity;
use super::{
    MAX_ROW_WALK_NESTING, PendingChange, RowHome, RowMap, RowSnapshot, Storage, VisibleRowHome,
};
use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

pub(super) struct RetainedRowIds {
    pool: Arc<RowWalkPool>,
    slot: usize,
}

impl core::fmt::Debug for RetainedRowIds {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("RetainedRowIds")
            .field("slot", &self.slot)
            .finish_non_exhaustive()
    }
}

impl RetainedRowIds {
    fn read(&self) -> RwLockReadGuard<'_, FixedVec<u64>> {
        self.pool.slots[self.slot]
            .rowids
            .read()
            .expect("row walk slot lock poisoned")
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        RowIdIter {
            rowids: self.read(),
            next: 0,
        }
    }

    #[cfg(test)]
    pub(super) fn contains(&self, rowid: u64) -> bool {
        self.read().binary_search(&rowid).is_ok()
    }

    fn sort(&mut self) {
        self.pool.slots[self.slot]
            .rowids
            .write()
            .expect("row walk slot lock poisoned")
            .as_mut_slice()
            .sort_unstable();
    }
}

impl Drop for RetainedRowIds {
    fn drop(&mut self) {
        let slot = &self.pool.slots[self.slot];
        slot.rowids
            .write()
            .expect("row walk slot lock poisoned")
            .clear();
        slot.retained.store(false, Ordering::Release);
    }
}

struct RowIdIter<'a> {
    rowids: RwLockReadGuard<'a, FixedVec<u64>>,
    next: usize,
}

impl Iterator for RowIdIter<'_> {
    type Item = u64;

    fn next(&mut self) -> Option<Self::Item> {
        let rowid = self.rowids.get(self.next).copied()?;
        self.next += 1;
        Some(rowid)
    }
}

pub(super) enum RowScanIds<'scan> {
    Reserved(RetainedRowIds),
    Statement(&'scan [u64]),
    Captured(&'scan [super::row_images::CapturedRowRecord]),
}

enum RowScanIdView<'a> {
    Reserved(RwLockReadGuard<'a, FixedVec<u64>>),
    Statement(&'a [u64]),
    Captured(&'a [super::row_images::CapturedRowRecord]),
}

impl RowScanIdView<'_> {
    fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        let len = match self {
            Self::Reserved(rowids) => rowids.len(),
            Self::Statement(rowids) => rowids.len(),
            Self::Captured(rows) => rows.len(),
        };
        (0..len).map(|index| match self {
            Self::Reserved(rowids) => rowids[index],
            Self::Statement(rowids) => rowids[index],
            Self::Captured(rows) => rows[index].rowid,
        })
    }
}

impl RowScanIds<'_> {
    fn read(&self) -> RowScanIdView<'_> {
        match self {
            Self::Reserved(rowids) => RowScanIdView::Reserved(rowids.read()),
            Self::Statement(rowids) => RowScanIdView::Statement(rowids),
            Self::Captured(rows) => RowScanIdView::Captured(rows),
        }
    }

    pub(super) fn contains(&self, rowid: u64) -> bool {
        match self {
            Self::Reserved(rowids) => rowids.read().binary_search(&rowid).is_ok(),
            Self::Statement(rowids) => rowids.binary_search(&rowid).is_ok(),
            Self::Captured(rows) => rows.binary_search_by_key(&rowid, |row| row.rowid).is_ok(),
        }
    }
}

/// Both scan phases share the partition captured before any callback runs.
pub(crate) struct RowScan<'storage> {
    storage: &'storage Storage,
    table_slot: usize,
    created_at: u64,
    pub(super) rowids: RowScanIds<'storage>,
    pub(super) roots: super::spill_generations::CapturedSpillRoots<'storage>,
}

/// An admitted scan with no resident overlay needs only its saved-file owners.
pub(crate) struct ImmutableRowScan<'arena> {
    roots: super::spill_generations::CapturedSpillRoots<'arena>,
}

impl ImmutableRowScan<'_> {
    #[cfg(test)]
    pub(crate) fn for_each_spilled_row_batch<'arena, 'callback>(
        &self,
        arena: &'arena crate::mem::arena::Arena,
        recycle_rows: bool,
        decoded_columns: Option<u64>,
        each: &mut super::SpilledRowBatchVisitor<'arena, 'callback>,
    ) -> Result<(), SqlError> {
        Storage::for_each_spilled_row_batch(
            &self.roots,
            &RowScanIds::Statement(&[]),
            arena,
            recycle_rows,
            decoded_columns,
            each,
        )
    }
}

impl Storage {
    /// Capture only copies fixed metadata and retains roots. Guard order is
    /// definition, publication, generation list, then row map; no I/O or user
    /// callback may run in this transition.
    pub(super) fn capture_row_partition<T>(
        &self,
        table_slot: usize,
        commit_lsn: u64,
        capture: impl FnOnce(
            super::spill_generations::SpillGenerationImage,
            &super::spill_generations::SpillGenerationList,
            &crate::mem::fixed_map::FixedMap<u64, super::RowState>,
        ) -> Result<T, SqlError>,
    ) -> Result<T, SqlError> {
        let table = &self.tables[table_slot];
        let definition = table.definition.read();
        let publication = table.publication();
        let generations = table.spill_generations();
        let rows = table.rows.read();
        let image = super::spill_generations::SpillGenerationImage::capture(
            &definition,
            publication.generation,
            table_slot,
            commit_lsn,
        );
        capture(image, &generations, &rows)
    }

    pub(crate) fn row_scan(&self, table_slot: usize) -> Result<RowScan<'_>, SqlError> {
        self.reserved_row_scan(table_slot, true)
    }

    pub(super) fn metadata_row_scan(&self, table_slot: usize) -> Result<RowScan<'_>, SqlError> {
        self.reserved_row_scan(table_slot, false)
    }

    fn reserved_row_scan(
        &self,
        table_slot: usize,
        skip_redundant: bool,
    ) -> Result<RowScan<'_>, SqlError> {
        let mut scan = self.capture_row_partition(
            table_slot,
            self.commit_snapshot(),
            |image, generations, rows| {
                let (rowids, _) = self.row_walks.retain_from(rows, skip_redundant)?;
                let roots = super::spill_generations::CapturedSpillRoots::capture(
                    self,
                    image,
                    generations,
                    None,
                )?;
                Ok(RowScan {
                    storage: self,
                    table_slot,
                    created_at: image.created_at,
                    rowids: RowScanIds::Reserved(rowids),
                    roots,
                })
            },
        )?;
        if let RowScanIds::Reserved(rowids) = &mut scan.rowids {
            rowids.sort();
        }
        Ok(scan)
    }

    #[cfg(test)]
    pub(crate) fn row_scan_in<'scan>(
        &'scan self,
        table_slot: usize,
        arena: &'scan crate::mem::arena::Arena,
    ) -> Result<RowScan<'scan>, SqlError> {
        let (image, rowids, roots) = self.capture_row_partition(
            table_slot,
            self.commit_snapshot(),
            |image, generations, rows| {
                let count = rows
                    .iter()
                    .filter(|(_, state)| !Self::redundant_spilled_row_state(state))
                    .count();
                let rowids = arena.alloc_slice_with(count, |_| 0u64).map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "retained byte scan identities exceed the statement arena"
                    )
                })?;
                let mut index = 0;
                for (&rowid, state) in rows.iter() {
                    if !Self::redundant_spilled_row_state(state) {
                        rowids[index] = rowid;
                        index += 1;
                    }
                }
                let roots = super::spill_generations::CapturedSpillRoots::capture(
                    self,
                    image,
                    generations,
                    Some(arena),
                )?;
                Ok((image, rowids, roots))
            },
        )?;
        rowids.sort_unstable();
        Ok(RowScan {
            storage: self,
            table_slot,
            created_at: image.created_at,
            rowids: RowScanIds::Statement(rowids),
            roots,
        })
    }

    pub(crate) fn immutable_row_scan_in<'scan>(
        &self,
        table_slot: usize,
        arena: &'scan crate::mem::arena::Arena,
    ) -> Result<Option<ImmutableRowScan<'scan>>, SqlError> {
        self.capture_immutable_row_scan(table_slot, Some(arena))
    }

    fn capture_immutable_row_scan<'scan>(
        &self,
        table_slot: usize,
        arena: Option<&'scan crate::mem::arena::Arena>,
    ) -> Result<Option<ImmutableRowScan<'scan>>, SqlError> {
        self.capture_row_partition(
            table_slot,
            self.commit_snapshot(),
            |image, generations, rows| {
                if !rows.is_empty() {
                    return Ok(None);
                }
                let roots = super::spill_generations::CapturedSpillRoots::capture(
                    self,
                    image,
                    generations,
                    arena,
                )?;
                Ok(Some(ImmutableRowScan { roots }))
            },
        )
    }

    #[cfg(test)]
    pub(super) fn immutable_row_scan(
        &self,
        table_slot: usize,
    ) -> Result<Option<ImmutableRowScan<'static>>, SqlError> {
        self.capture_immutable_row_scan(table_slot, None)
    }
}

impl RowScan<'_> {
    fn validate_identity(&self) -> Result<(), SqlError> {
        if self.storage.tables[self.table_slot].created_at() != self.created_at {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "retained scan table identity changed"
            ));
        }
        Ok(())
    }

    pub(crate) fn for_each_overlay(
        &self,
        each: &mut super::RowReadVisitor<'_>,
    ) -> Result<core::ops::ControlFlow<()>, SqlError> {
        self.validate_identity()?;
        for rowid in self.rowids.read().iter() {
            self.validate_identity()?;
            let state = match self.storage.resident_row_state(self.table_slot, rowid)? {
                Some(state) => Some(state.with_captured_roots(&self.roots)),
                None => {
                    let Some(roots) = self.roots.owned_source(self.storage)? else {
                        continue;
                    };
                    let version = roots.with_snapshot(|snapshot| {
                        Storage::probe_spill_snapshot(snapshot, rowid, u64::MAX)
                    })?;
                    version.map(|version| {
                        super::RowRead::immutable(super::SpilledRowState::new(
                            roots, rowid, version,
                        ))
                    })
                }
            };
            if let Some(state) = state
                && each(rowid, state)?.is_break()
            {
                return Ok(core::ops::ControlFlow::Break(()));
            }
        }
        Ok(core::ops::ControlFlow::Continue(()))
    }

    pub(super) fn for_each_spilled_state(
        &self,
        each: &mut super::RowReadVisitor<'_>,
    ) -> Result<(), SqlError> {
        self.validate_identity()?;
        let Some(roots) = self.roots.owned_source(self.storage)? else {
            return Ok(());
        };
        Storage::spill_merged_walk(&roots, &self.rowids, &mut |rowid, source| {
            each(rowid, super::RowRead::immutable(source))
        })
    }

    pub(crate) fn for_each_spilled_row_batch<'arena, 'callback>(
        &self,
        arena: &'arena crate::mem::arena::Arena,
        recycle_rows: bool,
        decoded_columns: Option<u64>,
        each: &mut super::SpilledRowBatchVisitor<'arena, 'callback>,
    ) -> Result<(), SqlError> {
        self.validate_identity()?;
        Storage::for_each_spilled_row_batch(
            &self.roots,
            &self.rowids,
            arena,
            recycle_rows,
            decoded_columns,
            each,
        )
    }
}

type PendingRowVisitor<'a> = dyn FnMut(
        u64,
        PendingChange,
        Option<VisibleRowHome>,
    ) -> Result<core::ops::ControlFlow<()>, SqlError>
    + 'a;

pub(super) struct RowWalkPool {
    slots: FixedVec<RowWalkSlot>,
}

struct RowWalkSlot {
    retained: AtomicBool,
    rowids: RwLock<FixedVec<u64>>,
}

impl Storage {
    pub(crate) fn for_each_pending_row_change(
        &self,
        table: usize,
        each: &mut PendingRowVisitor<'_>,
    ) -> Result<(), SqlError> {
        let rowids = self.row_walks.retain(&self.tables[table].rows)?;
        for rowid in rowids.iter() {
            let Some(state) = self.resident_row_state(table, rowid)? else {
                continue;
            };
            let Some(change) = state.pending_last() else {
                continue;
            };
            let image = change
                .loc
                .map(|location| {
                    self.pin_visible_home(
                        table,
                        rowid,
                        RowSnapshot {
                            created_at: self.tables[table].created_at(),
                            version: RowVersionIdentity::Pending(
                                state.pending_head_identity().expect("pending head"),
                            ),
                        },
                        RowHome::Heap(location),
                    )
                })
                .transpose()?;
            drop(state);
            if each(rowid, change, image)?.is_break() {
                return Ok(());
            }
        }
        Ok(())
    }
}

impl RowWalkPool {
    pub(super) fn budget_bytes(config: &Config) -> usize {
        size_of::<Self>()
            + 2 * size_of::<usize>()
            + Self::slot_count(config)
                * (size_of::<RowWalkSlot>() + Self::row_capacity(config) * size_of::<u64>())
    }

    fn slot_count(config: &Config) -> usize {
        config.query_workspace_slots * MAX_ROW_WALK_NESTING
    }

    fn row_capacity(config: &Config) -> usize {
        config
            .table_rows
            .max(config.large_object_pages)
            .max(config.value_index_rows)
    }

    pub(super) fn new(config: &Config, budget: &mut Budget) -> Result<Arc<Self>, BudgetError> {
        budget.draw(size_of::<Self>() + 2 * size_of::<usize>(), "row walk owner")?;
        let count = Self::slot_count(config);
        let mut slots = FixedVec::new(budget, "row walk slots", count)?;
        for _ in 0..count {
            slots
                .push(RowWalkSlot {
                    retained: AtomicBool::new(false),
                    rowids: RwLock::new(FixedVec::new(
                        budget,
                        "retained row walk identities",
                        Self::row_capacity(config),
                    )?),
                })
                .expect("sized to row walk slots");
        }
        Ok(Arc::new(Self { slots }))
    }

    #[cfg(test)]
    fn retain_scan(self: &Arc<Self>, rows: &RowMap) -> Result<(RetainedRowIds, bool), SqlError> {
        let (mut rowids, immutable_only) = self.retain_from(&rows.read(), true)?;
        rowids.sort();
        Ok((rowids, immutable_only))
    }

    fn retain_from(
        self: &Arc<Self>,
        rows: &crate::mem::fixed_map::FixedMap<u64, super::RowState>,
        skip_redundant: bool,
    ) -> Result<(RetainedRowIds, bool), SqlError> {
        let retained = self.capture(|rowids| {
            for (&rowid, state) in rows.iter() {
                if !skip_redundant || !Storage::redundant_spilled_row_state(state) {
                    rowids
                        .push(rowid)
                        .map_err(|error| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "{}", error))?;
                }
            }
            Ok(())
        })?;
        Ok((retained, rows.is_empty()))
    }

    fn capture(
        self: &Arc<Self>,
        fill: impl FnOnce(&mut FixedVec<u64>) -> Result<(), SqlError>,
    ) -> Result<RetainedRowIds, SqlError> {
        let slot = self
            .slots
            .iter()
            .position(|slot| {
                slot.retained
                    .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
            })
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "row walk retention slots are exhausted ({})",
                    self.slots.len()
                )
            })?;
        let retained = RetainedRowIds {
            pool: Arc::clone(self),
            slot,
        };
        {
            let mut rowids = self.slots[slot]
                .rowids
                .write()
                .expect("row walk slot lock poisoned");
            rowids.clear();
            fill(&mut rowids)?;
        }
        Ok(retained)
    }

    pub(super) fn retain_value_probe(
        self: &Arc<Self>,
        index: &crate::mem::value_index::ValueIndex,
        hash: u64,
    ) -> Result<RetainedRowIds, SqlError> {
        self.capture(|rowids| {
            let mut error = None;
            index.probe(hash, |rowid| {
                if error.is_none()
                    && let Err(full) = rowids.push(rowid)
                {
                    error = Some(full);
                }
            });
            if let Some(error) = error {
                return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "{}", error));
            }
            Ok(())
        })
    }

    pub(super) fn retain(self: &Arc<Self>, rows: &RowMap) -> Result<RetainedRowIds, SqlError> {
        let (mut rowids, _) = self.retain_from(&rows.read(), false)?;
        rowids.sort();
        Ok(rowids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::types::{ColType, Datum};
    use crate::storage::tests::{make_def, test_budget, test_config};
    use crate::storage::{RowState, SNAPSHOT_ALL};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    fn fixture() -> (Storage, usize) {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let table = storage
            .create_table(make_def("retained_walk", &[("id", ColType::Int4, false)]))
            .unwrap();
        let location = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        storage.tables[table]
            .rows
            .insert(1, RowState::committed_only_at(location, 7))
            .unwrap();
        (storage, table)
    }

    #[test]
    fn owned_row_walk_identities_outlive_map_and_pool_owners() {
        let mut config = test_config();
        config.table_rows = 3;
        config.large_object_pages = 3;
        config.value_index_rows = 3;
        let mut budget = Budget::new(RowWalkPool::budget_bytes(&config));
        let pool = RowWalkPool::new(&config, &mut budget).unwrap();
        assert_eq!(budget.remaining(), 0);
        let mut row_budget = Budget::new(4096);
        let mut rows = RowMap::new(&mut row_budget, "owned walk rows", 3).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            for rowid in [3, 1, 2] {
                rows.insert(
                    rowid,
                    RowState::committed_only_at(super::super::RowLoc::test(0, 4), 7),
                )
                .unwrap();
            }
            let retained = pool.retain(&rows).unwrap();
            rows.clear();
            rows.insert(
                9,
                RowState::committed_only_at(super::super::RowLoc::test(0, 4), 8),
            )
            .unwrap();
            drop(rows);
            drop(pool);
            let mut iterator = retained.iter();
            for expected in [1, 2, 3] {
                assert_eq!(iterator.next(), Some(expected));
                assert!(
                    retained.contains(expected),
                    "nested reads share an immutable slot"
                );
            }
            assert_eq!(iterator.next(), None);
            drop(iterator);
            assert_eq!(retained.iter().count(), 3);
            drop(retained);
        });
    }

    #[test]
    fn owned_row_walk_identities_survive_table_reuse_and_storage_shutdown() {
        let (mut storage, table) = fixture();
        crate::mem::guard::forbid_alloc(|| {
            let retained = storage
                .row_walks
                .retain(&storage.tables[table].rows)
                .unwrap();
            storage.commit_drop(table);
            let replacement = storage
                .create_table(make_def(
                    "owned_walk_replacement",
                    &[("id", ColType::Text, true)],
                ))
                .unwrap();
            assert_eq!(replacement, table);
            let location = storage.heap.append_row(&[Datum::Text("new")]).unwrap();
            storage.tables[table]
                .rows
                .insert(2, RowState::committed_only_at(location, 8))
                .unwrap();
            drop(storage);
            let partition = RowScanIds::Reserved(retained);
            assert!(partition.contains(1));
            assert!(!partition.contains(2));
            assert!(partition.read().iter().eq([1]));
            drop(partition);
        });
    }

    #[test]
    fn owned_row_walk_identities_parallel_capture_and_release_keep_slots_distinct() {
        let mut config = test_config();
        config.table_rows = 2;
        config.large_object_pages = 2;
        config.value_index_rows = 2;
        let mut budget = Budget::new(RowWalkPool::budget_bytes(&config));
        let pool = RowWalkPool::new(&config, &mut budget).unwrap();
        let mut row_budget = Budget::new(8192);
        let mut left = RowMap::new(&mut row_budget, "left owned walk", 2).unwrap();
        let mut right = RowMap::new(&mut row_budget, "right owned walk", 2).unwrap();
        for rowid in [1, 2] {
            left.insert(
                rowid,
                RowState::committed_only_at(super::super::RowLoc::test(0, 4), 7),
            )
            .unwrap();
            right
                .insert(
                    rowid + 6,
                    RowState::committed_only_at(super::super::RowLoc::test(0, 4), 7),
                )
                .unwrap();
        }
        let together = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let worker = |rows: &RowMap, first| {
                crate::mem::guard::forbid_alloc(|| {
                    let retained = pool.retain(rows).unwrap();
                    together.wait();
                    let slot = retained.slot;
                    for _ in 0..100 {
                        let mut iterator = retained.iter();
                        assert_eq!(iterator.next(), Some(first));
                        assert!(retained.contains(first + 1));
                        assert_eq!(iterator.next(), Some(first + 1));
                        assert_eq!(iterator.next(), None);
                    }
                    together.wait();
                    slot
                })
            };
            let left_rows = &left;
            let right_rows = &right;
            let left = scope.spawn(move || worker(left_rows, 1));
            let right = scope.spawn(move || worker(right_rows, 7));
            assert_ne!(left.join().unwrap(), right.join().unwrap());
        });
        assert!(
            pool.slots
                .iter()
                .all(|slot| !slot.retained.load(Ordering::Acquire))
        );
        crate::mem::guard::forbid_alloc(|| {
            let retained = pool.retain(&left).unwrap();
            assert_eq!(retained.iter().count(), 2);
        });
    }

    #[test]
    fn retained_row_walk_allows_publication_during_a_pinned_callback() {
        let (storage, table) = fixture();
        let start = AtomicBool::new(false);
        let completed = AtomicBool::new(false);
        let mut progressed = false;
        std::thread::scope(|scope| {
            let storage = &storage;
            let start = &start;
            let completed = &completed;
            let writer = scope.spawn(move || {
                while !start.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                crate::mem::guard::forbid_alloc(|| {
                    let next = storage.heap.append_row(&[Datum::Int4(2)]).unwrap();
                    let undo = storage.write_pending(table, 2, 7, 1, Some(next)).unwrap();
                    completed.store(true, Ordering::Release);
                    undo
                })
            });
            crate::mem::guard::forbid_alloc(|| {
                let mut count = 0;
                storage
                    .for_each_row_state(table, &mut |rowid, state| {
                        assert_eq!(rowid, 1);
                        let image = storage
                            .visible_row_home_at(table, rowid, state, 8, SNAPSHOT_ALL, 7)?
                            .unwrap();
                        assert!(storage.tables[table].rows.test_write().is_ok());
                        assert!(storage.row_versions.test_write().is_ok());
                        start.store(true, Ordering::Release);
                        let deadline = Instant::now() + Duration::from_secs(5);
                        while !completed.load(Ordering::Acquire) && Instant::now() < deadline {
                            std::thread::yield_now();
                        }
                        progressed = completed.load(Ordering::Acquire);
                        storage.with_row_bytes(table, rowid, image, |bytes| {
                            let mut values = [Datum::Null];
                            super::super::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                            assert_eq!(values[0], Datum::Int4(1));
                            Ok(())
                        })?;
                        count += 1;
                        Ok(core::ops::ControlFlow::Continue(()))
                    })
                    .unwrap();
                assert_eq!(
                    count, 1,
                    "a later insertion is outside the retained row set"
                );
            });
            let undo = writer.join().unwrap();
            assert!(
                progressed,
                "writer must finish while the scan callback retains bytes"
            );
            storage.restore_pending(table, 2, 7, undo);
        });
    }

    fn checkpoint_fixture() -> (Storage, usize) {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let mut definition = make_def("checkpoint_walk", &[("id", ColType::Int4, true)]);
        definition.columns[0].primary = true;
        definition.columns[0].unique = true;
        let table = storage.create_table(definition).unwrap();
        for rowid in 1..=2 {
            let location = storage
                .heap
                .append_row(&[Datum::Int4(rowid as i32)])
                .unwrap();
            storage.tables[table]
                .rows
                .insert(rowid, RowState::committed_only_at(location, 7))
                .unwrap();
        }
        storage.refresh_enforcers(table).unwrap();
        (storage, table)
    }

    #[test]
    fn historical_sst_retirement_remaps_every_version_home() {
        use super::super::CommittedVersion;
        use super::super::row_versions::push_committed_version;
        use crate::store::{BlockId, RowSstFormat, SstHandle};

        let handle = |tag| SstHandle {
            index: BlockId([tag; 32]),
            filter: BlockId([tag + 1; 32]),
            roster: BlockId([tag + 2; 32]),
            format: RowSstFormat::PackedV4,
        };
        let spilled = |sst, lsn| RowHome::Spilled {
            len: 16,
            sst,
            commit_lsn: lsn,
        };
        for at in [0, 1] {
            let (mut storage, table) = checkpoint_fixture();
            storage.set_spill_list(table, &[handle(1), handle(5), handle(9)]);
            let mut state = RowState::committed_only_at(super::super::RowLoc::test(1, 4), 11);
            state.committed = Some(spilled(2, 11));
            let versions = &mut storage.row_versions.exclusive().committed_row_versions;
            for (home, lsn) in [
                (Some(RowHome::Heap(super::super::RowLoc::test(1, 4))), 3),
                (None, 5),
                (Some(spilled(0, 7)), 7),
                (Some(spilled(1, 9)), 9),
            ] {
                push_committed_version(
                    versions,
                    &mut state.history,
                    4,
                    CommittedVersion { home, lsn },
                )
                .unwrap();
            }
            storage.tables[table].rows.remove(&1);
            storage.tables[table].rows.insert(1, state).unwrap();
            crate::mem::guard::forbid_alloc(|| {
                storage.merge_spill_pair(table, at, Some(handle(13)));
                let state = storage.resident_row_state(table, 1).unwrap().unwrap();
                assert_eq!(state.committed, Some(spilled(1, 11)));
                assert_eq!(
                    state.history_get(0).unwrap().home,
                    Some(spilled(at as u32, 9))
                );
                assert_eq!(state.history_get(1).unwrap().home, Some(spilled(0, 7)));
                assert_eq!(state.history_get(2).unwrap().home, None);
                assert_eq!(
                    state.history_get(3).unwrap().home,
                    Some(RowHome::Heap(super::super::RowLoc::test(1, 4)))
                );
                assert_eq!(state.history_get(0).unwrap().lsn, 9);
                assert_eq!(state.committed_lsn, 11);
                drop(state);
                storage.collapse_spill(table, handle(17)).unwrap();
                let state = storage.resident_row_state(table, 1).unwrap().unwrap();
                assert_eq!(state.committed, Some(spilled(0, 11)));
                assert_eq!(state.history_get(0).unwrap().home, Some(spilled(0, 9)));
                assert_eq!(state.history_get(1).unwrap().home, Some(spilled(0, 7)));
                assert_eq!(state.history_get(2).unwrap().home, None);
                assert_eq!(state.history_get(3).unwrap().lsn, 3);
                assert_eq!(state.checkpoint_change_lsn, 11);
                assert!(state.pending.is_none());
            });
        }
    }

    #[test]
    fn shared_committed_publication_progresses_during_a_retained_value_probe() {
        let (storage, table) = checkpoint_fixture();
        storage.register_snapshot(99, 7).unwrap();
        storage.set_commit_snapshot(7);
        let mut hashes = [(0, 0); super::super::MAX_VALUE_ENFORCERS];
        let state = storage.resident_row_state(table, 1).unwrap().unwrap();
        let home = storage
            .committed_row_home(table, 1, state)
            .unwrap()
            .unwrap();
        assert_eq!(
            storage
                .row_enforcer_hashes(table, 1, home, &mut hashes)
                .unwrap(),
            1
        );
        let start = AtomicBool::new(false);
        let completed = AtomicBool::new(false);
        let mut progressed = false;
        std::thread::scope(|scope| {
            let storage = &storage;
            let start = &start;
            let completed = &completed;
            let writer = scope.spawn(move || {
                while !start.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                crate::mem::guard::forbid_alloc(|| {
                    let location = storage.heap.append_row(&[Datum::Int4(20)]).unwrap();
                    storage
                        .write_pending(table, 1, 8, 1, Some(location))
                        .unwrap();
                    storage.commit_row(table, 1, 8, 9).unwrap();
                    completed.store(true, Ordering::Release);
                });
            });
            crate::mem::guard::forbid_alloc(|| {
                assert!(
                    storage
                        .probe_value(table, &[0], hashes[0].1, |rowid, _| {
                            let state = storage.resident_row_state(table, rowid).unwrap().unwrap();
                            let image = storage
                                .visible_row_home_at(table, rowid, state, 99, SNAPSHOT_ALL, 7)?
                                .unwrap();
                            assert!(storage.tables[table].rows.test_write().is_ok());
                            assert!(storage.row_versions.test_write().is_ok());
                            start.store(true, Ordering::Release);
                            let deadline = Instant::now() + Duration::from_secs(5);
                            while !completed.load(Ordering::Acquire) && Instant::now() < deadline {
                                std::thread::yield_now();
                            }
                            progressed = completed.load(Ordering::Acquire);
                            storage.with_row_bytes(table, rowid, image, |bytes| {
                                let mut values = [Datum::Null];
                                super::super::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                                assert_eq!(values[0], Datum::Int4(1));
                                Ok(())
                            })?;
                            Ok(())
                        })
                        .unwrap()
                );
            });
            writer.join().unwrap();
        });
        assert!(
            progressed,
            "commit must complete while the probe retains the old image"
        );
        crate::mem::guard::forbid_alloc(|| {
            assert!(storage.value_cache_complete(table, &[0]));
            assert!(!storage.value_probe_complete(table, &[0]));
            assert!(
                !storage
                    .probe_value(table, &[0], hashes[0].1, |_, _| panic!(
                        "a newer cache cannot certify an old snapshot"
                    ))
                    .unwrap()
            );
            let state = storage.resident_row_state(table, 1).unwrap().unwrap();
            assert_eq!(state.committed_lsn, 9);
            assert_eq!(state.history.len, 1);
            drop(state);
            storage.release_snapshot(99);
            assert!(
                storage
                    .resident_row_state(table, 1)
                    .unwrap()
                    .unwrap()
                    .history
                    .is_empty()
            );
        });
    }

    #[test]
    fn shared_committed_publication_rebuilt_cache_retains_deletion_boundary() {
        let (mut storage, table) = checkpoint_fixture();
        storage.register_snapshot(99, 7).unwrap();
        storage.set_commit_snapshot(7);
        crate::mem::guard::forbid_alloc(|| {
            storage.write_pending(table, 1, 8, 1, None).unwrap();
            storage.commit_row(table, 1, 8, 9).unwrap();
        });
        storage.refresh_enforcers(table).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            assert!(storage.value_cache_complete(table, &[0]));
            assert!(
                !storage.value_probe_complete(table, &[0]),
                "a rebuilt cache must include the deletion boundary"
            );
            let state = storage.resident_row_state(table, 1).unwrap().unwrap();
            let image = storage
                .visible_row_home_at(table, 1, state, 99, SNAPSHOT_ALL, 7)
                .unwrap()
                .unwrap();
            storage
                .with_row_bytes(table, 1, image, |bytes| {
                    let mut values = [Datum::Null];
                    super::super::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                    assert_eq!(values[0], Datum::Int4(1));
                    Ok(())
                })
                .unwrap();
            storage.release_snapshot(99);
            assert!(storage.resident_row_state(table, 1).unwrap().is_none());
        });
    }

    #[test]
    fn shared_committed_publication_exhaustion_preserves_rows_and_cache() {
        let (storage, table) = checkpoint_fixture();
        let location = storage.heap.append_row(&[Datum::Int4(20)]).unwrap();
        storage
            .write_pending(table, 1, 8, 1, Some(location))
            .unwrap();
        storage.tables[table].publication().generation = u64::MAX;
        let binding = storage.tables[table].enforcer(0).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let error = storage.commit_row(table, 1, 8, 9).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            let row = storage.resident_row_state(table, 1).unwrap().unwrap();
            assert_eq!(row.committed_lsn, 7);
            assert_eq!(row.pending_last().unwrap().loc, Some(location));
            drop(row);
            let retained = storage.tables[table].enforcer(0).unwrap();
            assert_eq!(retained.durable_dirty_lsn, binding.durable_dirty_lsn);
            assert!(storage.row_versions.test_write().is_ok());
            assert!(storage.tables[table].rows.test_write().is_ok());
        });
    }

    #[test]
    fn checkpoint_retained_rows_resume_after_rollback_and_later_publication() {
        let (storage, table) = checkpoint_fixture();
        let location = storage.heap.append_row(&[Datum::Int4(99)]).unwrap();
        let undone = storage
            .write_pending(table, 3, 7, 1, Some(location))
            .unwrap();
        let mut cursor = super::super::CheckpointValueCursor::new(1, 128);
        let mut budget = Budget::new(4096);
        let arena = crate::mem::arena::Arena::new(&mut budget, "checkpoint test", 4096).unwrap();
        let mut output = [0u8; 128];
        crate::mem::guard::forbid_alloc(|| {
            let mut seen = [0u64; 2];
            let mut count = 0;
            let mut each = |entry: super::super::CheckpointValueEntry<'_>| {
                assert!(storage.tables[table].rows.test_write().is_ok());
                assert!(storage.row_versions.test_write().is_ok());
                assert!(storage.heap.test_write().is_ok());
                assert_eq!(entry.commit_lsn, 7);
                let mut values = [Datum::Null];
                super::super::rowenc::decode(entry.key, &[ColType::Int4], &mut values)?;
                assert_eq!(values[0], Datum::Int4(entry.rowid as i32));
                seen[count] = entry.rowid;
                count += 1;
                Ok(core::ops::ControlFlow::Continue(()))
            };
            assert!(
                !storage
                    .for_each_value_binding_entry_batch(
                        table,
                        0,
                        &mut cursor,
                        &arena,
                        &mut output,
                        1,
                        8,
                        &mut each
                    )
                    .unwrap()
            );
            storage.restore_pending(table, 3, 7, undone);
            let later = storage
                .write_pending(table, 4, 7, 1, Some(location))
                .unwrap();
            let replacement = storage
                .write_pending(table, 2, 7, 1, Some(location))
                .unwrap();
            while !storage
                .for_each_value_binding_entry_batch(
                    table,
                    0,
                    &mut cursor,
                    &arena,
                    &mut output,
                    1,
                    8,
                    &mut each,
                )
                .unwrap()
            {}
            assert_eq!(seen, [1, 2]);
            assert_eq!(count, 2);
            storage.restore_pending(table, 2, 7, replacement);
            storage.restore_pending(table, 4, 7, later);
        });
    }

    #[test]
    fn checkpoint_retained_delta_rows_keep_tombstones_and_release_callbacks() {
        let (mut storage, table) = checkpoint_fixture();
        let mut deleted = *storage.resident_row_state(table, 1).unwrap().unwrap();
        deleted.committed = None;
        deleted.committed_lsn = 9;
        storage.tables[table].rows.insert(3, deleted).unwrap();
        let mut cursor = super::super::CheckpointValueCursor::new(1, 128);
        let mut changed = Vec::with_capacity(128);
        let mut output = [0u8; 128];
        crate::mem::guard::forbid_alloc(|| {
            let mut count = 0;
            let mut each = |entry: super::super::CheckpointValueEntry<'_>| {
                assert!(storage.tables[table].rows.test_write().is_ok());
                assert!(storage.row_versions.test_write().is_ok());
                let location = storage.heap.append_row(&[Datum::Int4(99)])?;
                let undo = storage.write_pending(table, entry.rowid, 7, 1, Some(location))?;
                storage.restore_pending(table, entry.rowid, 7, undo);
                count += 1;
                Ok(core::ops::ControlFlow::Break(()))
            };
            while !storage
                .for_each_value_binding_delta_entry_batch(
                    table,
                    0,
                    6,
                    &mut cursor,
                    &mut changed,
                    &mut output,
                    1,
                    &mut each,
                )
                .unwrap()
            {}
            assert_eq!(count, 2);
            assert_eq!(changed, [1, 2, 3]);
            assert_eq!(
                storage
                    .resident_row_state(table, 3)
                    .unwrap()
                    .unwrap()
                    .committed_lsn,
                9
            );
        });
    }

    #[test]
    fn checkpoint_retained_rows_capacity_and_error_reset_are_allocation_free() {
        let (storage, table) = checkpoint_fixture();
        let mut cursor = super::super::CheckpointValueCursor::new(1, 1);
        let mut changed = Vec::with_capacity(128);
        let mut output = [0u8; 128];
        crate::mem::guard::forbid_alloc(|| {
            let error = storage
                .for_each_value_binding_delta_entry_batch(
                    table,
                    0,
                    6,
                    &mut cursor,
                    &mut changed,
                    &mut output,
                    1,
                    &mut |_| Ok(core::ops::ControlFlow::Continue(())),
                )
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(
                error
                    .message
                    .as_str()
                    .contains("checkpoint retained row identities")
            );
            assert!(cursor.resident_rowids.is_empty());
            assert!(!cursor.resident_initialized);
            assert!(storage.tables[table].rows.test_write().is_ok());
        });
        let mut cursor = super::super::CheckpointValueCursor::new(1, 128);
        crate::mem::guard::forbid_alloc(|| {
            let error = storage
                .for_each_value_binding_delta_entry_batch(
                    table,
                    0,
                    6,
                    &mut cursor,
                    &mut changed,
                    &mut output,
                    1,
                    &mut |_| {
                        Err(sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "checkpoint callback failure"
                        ))
                    },
                )
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::INTERNAL_ERROR);
            assert!(storage.tables[table].rows.test_write().is_ok());
            assert!(storage.row_versions.test_write().is_ok());
            assert!(storage.heap.test_write().is_ok());
            cursor.reset();
            changed.clear();
            let mut count = 0;
            assert!(
                storage
                    .for_each_value_binding_delta_entry_batch(
                        table,
                        0,
                        6,
                        &mut cursor,
                        &mut changed,
                        &mut output,
                        128,
                        &mut |_| {
                            count += 1;
                            Ok(core::ops::ControlFlow::Continue(()))
                        }
                    )
                    .unwrap()
            );
            assert_eq!(count, 2);
            assert_eq!(changed, [1, 2]);
        });
    }

    #[test]
    fn retained_byte_scan_allows_publication_during_a_pinned_callback() {
        let (storage, table) = fixture();
        let start = AtomicBool::new(false);
        let completed = AtomicBool::new(false);
        let mut progressed = false;
        std::thread::scope(|scope| {
            let storage = &storage;
            let start = &start;
            let completed = &completed;
            let writer = scope.spawn(move || {
                while !start.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                crate::mem::guard::forbid_alloc(|| {
                    let next = storage.heap.append_row(&[Datum::Int4(2)]).unwrap();
                    let undo = storage.write_pending(table, 2, 7, 1, Some(next)).unwrap();
                    completed.store(true, Ordering::Release);
                    undo
                })
            });
            crate::mem::guard::forbid_alloc(|| {
                let mut count = 0;
                let scan = storage.row_scan(table).unwrap();
                let overlay = scan
                    .for_each_overlay(&mut |rowid, state| {
                        assert_eq!(rowid, 1);
                        let image = storage
                            .visible_row_home_at(table, rowid, state, 8, SNAPSHOT_ALL, 7)?
                            .unwrap();
                        assert!(storage.tables[table].rows.test_write().is_ok());
                        assert!(storage.row_versions.test_write().is_ok());
                        start.store(true, Ordering::Release);
                        let deadline = Instant::now() + Duration::from_secs(5);
                        while !completed.load(Ordering::Acquire) && Instant::now() < deadline {
                            std::thread::yield_now();
                        }
                        progressed = completed.load(Ordering::Acquire);
                        storage.with_row_bytes(table, rowid, image, |bytes| {
                            let mut values = [Datum::Null];
                            super::super::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                            assert_eq!(values[0], Datum::Int4(1));
                            Ok(())
                        })?;
                        count += 1;
                        Ok(core::ops::ControlFlow::Continue(()))
                    })
                    .unwrap();
                assert!(overlay.is_continue());
                assert_eq!(
                    count, 1,
                    "a later insertion is outside the retained row set"
                );
            });
            let undo = writer.join().unwrap();
            assert!(
                progressed,
                "writer must finish while the scan callback retains bytes"
            );
            storage.restore_pending(table, 2, 7, undo);
        });
    }

    #[test]
    fn retained_row_walk_survives_removal_reuse_and_nested_walks() {
        let (storage, table) = fixture();
        let location = storage.heap.append_row(&[Datum::Int4(2)]).unwrap();
        let undo = storage
            .write_pending(table, 2, 7, 1, Some(location))
            .unwrap();
        let mut seen = [0u64; 2];
        crate::mem::guard::forbid_alloc(|| {
            let mut count = 0;
            storage
                .for_each_row_state(table, &mut |rowid, state| {
                    drop(state);
                    seen[count] = rowid;
                    count += 1;
                    if rowid == 1 {
                        storage.restore_pending(table, 2, 7, undo);
                        let replacement = storage.write_pending(table, 3, 7, 1, Some(location))?;
                        let mut nested = [0u64; 2];
                        let mut n = 0;
                        storage.for_each_row_state(table, &mut |nested_rowid, state| {
                            drop(state);
                            nested[n] = nested_rowid;
                            n += 1;
                            Ok(core::ops::ControlFlow::Continue(()))
                        })?;
                        assert_eq!(nested, [1, 3]);
                        storage.restore_pending(table, 3, 7, replacement);
                    }
                    Ok(core::ops::ControlFlow::Continue(()))
                })
                .unwrap();
            assert_eq!(count, 1);
            assert_eq!(seen[0], 1);
            assert!(storage.tables[table].rows.test_write().is_ok());
            assert!(storage.row_versions.test_write().is_ok());
        });
    }

    #[test]
    fn retained_row_walk_pending_callbacks_release_chain_ownership() {
        let (storage, table) = fixture();
        let location = storage.heap.append_row(&[Datum::Int4(2)]).unwrap();
        let undo = storage
            .write_pending(table, 2, 7, 1, Some(location))
            .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mut count = 0;
            storage
                .for_each_pending_row_change(table, &mut |rowid, change, image| {
                    assert_eq!(rowid, 2);
                    assert_eq!(change.txid, 7);
                    assert!(storage.tables[table].rows.test_write().is_ok());
                    assert!(storage.row_versions.test_write().is_ok());
                    storage.restore_pending(table, rowid, 7, undo);
                    assert_eq!(change.loc, Some(location));
                    assert!(storage.resident_row_state(table, rowid).unwrap().is_none());
                    assert!(storage.heap.test_write().is_err());
                    storage.with_row_bytes(table, rowid, image.unwrap(), |bytes| {
                        let mut values = [Datum::Null];
                        super::super::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                        assert_eq!(values[0], Datum::Int4(2));
                        Ok(())
                    })?;
                    assert!(storage.heap.test_write().is_ok());
                    count += 1;
                    Ok(core::ops::ControlFlow::Continue(()))
                })
                .unwrap();
            assert_eq!(count, 1);
        });
    }

    #[test]
    fn retained_row_walk_committed_validation_pins_before_metadata_release() {
        let (storage, table) = fixture();
        let location = storage.heap.append_row(&[Datum::Int4(2)]).unwrap();
        let undo = storage
            .write_pending(table, 1, 7, 1, Some(location))
            .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let state = storage.resident_row_state(table, 1).unwrap().unwrap();
            let image = storage
                .committed_row_home(table, 1, state)
                .unwrap()
                .unwrap();
            assert!(storage.row_versions.test_write().is_ok());
            assert!(storage.tables[table].rows.test_write().is_ok());
            assert!(storage.heap.test_write().is_err());
            storage.restore_pending(table, 1, 7, undo);
            storage
                .with_row_bytes(table, 1, image, |bytes| {
                    let mut values = [Datum::Null];
                    super::super::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                    assert_eq!(values[0], Datum::Int4(1), "ignore the pending replacement");
                    Ok(())
                })
                .unwrap();
            assert!(storage.heap.test_write().is_ok());
        });
    }

    #[test]
    fn retained_byte_scan_filters_redundant_entries_and_releases_on_error() {
        let (mut storage, table) = fixture();
        let mut redundant = *storage.resident_row_state(table, 1).unwrap().unwrap();
        redundant.committed = Some(RowHome::Spilled {
            len: 4,
            sst: 0,
            commit_lsn: 7,
        });
        redundant.checkpoint_change_lsn = 0;
        storage.tables[table].rows.insert(2, redundant).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let scan = storage.row_scan(table).unwrap();
            assert!(scan.rowids.contains(1));
            assert!(!scan.rowids.contains(2));
            assert!(storage.immutable_row_scan(table).unwrap().is_none());
            let failure = scan.for_each_overlay(&mut |rowid, state| {
                assert_eq!(rowid, 1);
                drop(state);
                assert!(storage.tables[table].rows.test_write().is_ok());
                assert!(storage.row_versions.test_write().is_ok());
                let nested = storage.row_scan(table)?;
                assert!(nested.rowids.contains(1));
                Err(sql_err!(sqlstate::INTERNAL_ERROR, "scan callback failure"))
            });
            assert_eq!(failure.unwrap_err().sqlstate, sqlstate::INTERNAL_ERROR);
            assert!(
                scan.for_each_overlay(&mut |_, state| {
                    drop(state);
                    Ok(core::ops::ControlFlow::Break(()))
                })
                .unwrap()
                .is_break()
            );
            drop(scan);
            assert!(storage.row_scan(table).is_ok());
        });
    }

    #[test]
    fn retained_byte_scan_keeps_immutable_admission_after_a_later_insert() {
        let (mut storage, table) = fixture();
        storage.tables[table].rows.remove(&1);
        let mut budget = Budget::new(64 << 10);
        let arena = crate::mem::arena::Arena::new(&mut budget, "empty scan", 64 << 10).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let scan = storage.immutable_row_scan(table).unwrap().unwrap();
            let location = storage.heap.append_row(&[Datum::Int4(2)]).unwrap();
            let undo = storage
                .write_pending(table, 2, 7, 1, Some(location))
                .unwrap();
            assert!(storage.immutable_row_scan(table).unwrap().is_none());
            let mut count = 0;
            scan.for_each_spilled_row_batch(&arena, true, None, &mut |rows| {
                count += rows.len();
                Ok(core::ops::ControlFlow::Continue(()))
            })
            .unwrap();
            assert_eq!(count, 0, "later overlays cannot alter an admitted scan");
            storage.restore_pending(table, 2, 7, undo);
        });
    }

    #[test]
    fn retained_byte_scan_statement_arena_preserves_join_depth_and_exhaustion() {
        let (storage, table) = fixture();
        let count = MAX_ROW_WALK_NESTING + 1;
        let mut budget = Budget::new(count * size_of::<u64>());
        let arena = crate::mem::arena::Arena::new(
            &mut budget,
            "retained scan identities",
            count * size_of::<u64>(),
        )
        .unwrap();
        let mut scans = Vec::with_capacity(count);
        crate::mem::guard::forbid_alloc(|| {
            for _ in 0..count {
                scans.push(storage.row_scan_in(table, &arena).unwrap());
            }
            assert!(
                storage.row_scan(table).is_ok(),
                "SQL scans do not consume metadata walk leases"
            );
            assert_eq!(
                storage.row_scan_in(table, &arena).err().unwrap().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            for scan in &scans {
                assert!(scan.rowids.contains(1));
                assert!(
                    scan.for_each_overlay(&mut |rowid, state| {
                        assert_eq!(rowid, 1);
                        drop(state);
                        assert!(storage.tables[table].rows.test_write().is_ok());
                        Ok(core::ops::ControlFlow::Continue(()))
                    })
                    .unwrap()
                    .is_continue()
                );
            }
        });
    }

    #[test]
    fn retained_row_walk_budget_exhaustion_and_release_are_bounded() {
        let mut config = test_config();
        config.query_workspace_slots = 2;
        config.table_rows = 2;
        config.large_object_pages = 3;
        let bytes = RowWalkPool::budget_bytes(&config);
        let mut budget = Budget::new(bytes);
        let pool = RowWalkPool::new(&config, &mut budget).unwrap();
        assert_eq!(budget.remaining(), 0);
        let mut row_budget = Budget::new(4096);
        let rows = RowMap::new(&mut row_budget, "walk rows", 3).unwrap();
        let count = RowWalkPool::slot_count(&config);
        let mut leases = Vec::with_capacity(count);
        crate::mem::guard::forbid_alloc(|| {
            for _ in 0..count {
                leases.push(pool.retain_scan(&rows).unwrap().0);
            }
            assert_eq!(
                pool.retain_scan(&rows).unwrap_err().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            drop(leases.pop());
            assert!(pool.retain(&rows).is_ok());
            leases.clear();
            assert!(pool.retain(&rows).is_ok());
        });
    }

    #[test]
    fn retained_row_walk_failed_capture_releases_its_slot() {
        let mut config = test_config();
        config.table_rows = 1;
        config.large_object_pages = 1;
        config.value_index_rows = 1;
        let mut budget = Budget::new(RowWalkPool::budget_bytes(&config));
        let pool = RowWalkPool::new(&config, &mut budget).unwrap();
        let mut row_budget = Budget::new(4096);
        let mut rows = RowMap::new(&mut row_budget, "oversized fixture", 2).unwrap();
        rows.insert(
            1,
            RowState::committed_only_at(super::super::RowLoc::test(0, 4), 7),
        )
        .unwrap();
        rows.insert(
            2,
            RowState::committed_only_at(super::super::RowLoc::test(0, 4), 7),
        )
        .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let error = pool.retain(&rows).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(rows.test_write().is_ok());
            rows.remove(&2).unwrap();
            let retained = pool.retain(&rows).unwrap();
            assert_eq!(retained.iter().next(), Some(1));
            assert_eq!(retained.iter().count(), 1);
            assert!(retained.contains(1));
            assert!(!retained.contains(2));
        });
    }
}
