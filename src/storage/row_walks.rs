//! Startup-bounded row identity retention for authoritative metadata walks.

use core::mem::size_of;
use std::sync::{Mutex, MutexGuard};

use super::{MAX_ROW_WALK_NESTING, PendingChange, RowMap, Storage, try_mutex_pool};
use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

#[derive(Debug)]
pub(super) struct RetainedRowIds<'a> {
    rowids: MutexGuard<'a, FixedVec<u64>>,
}

impl RetainedRowIds<'_> {
    pub(super) fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        self.rowids.iter().copied()
    }

    pub(super) fn contains(&self, rowid: u64) -> bool {
        self.rowids.binary_search(&rowid).is_ok()
    }
}

pub(super) struct RowWalkPool {
    slots: FixedVec<Mutex<FixedVec<u64>>>,
}

impl Storage {
    pub(crate) fn for_each_pending_row_change(
        &self,
        table: usize,
        each: &mut dyn FnMut(u64, PendingChange) -> Result<core::ops::ControlFlow<()>, SqlError>,
    ) -> Result<(), SqlError> {
        let rowids = self.row_walks.retain(&self.tables[table].rows)?;
        for rowid in rowids.iter() {
            let change = self
                .resident_row_state(table, rowid)
                .and_then(|state| state.pending_last());
            if let Some(change) = change
                && each(rowid, change)?.is_break()
            {
                return Ok(());
            }
        }
        Ok(())
    }
}

impl RowWalkPool {
    pub(super) fn budget_bytes(config: &Config) -> usize {
        size_of::<Self>() + Self::slot_count(config)
            * (size_of::<Mutex<FixedVec<u64>>>() + Self::row_capacity(config) * size_of::<u64>())
    }

    fn slot_count(config: &Config) -> usize {
        config.query_workspace_slots * MAX_ROW_WALK_NESTING
    }

    fn row_capacity(config: &Config) -> usize {
        config.table_rows.max(config.large_object_pages)
    }

    pub(super) fn new(config: &Config, budget: &mut Budget) -> Result<Self, BudgetError> {
        budget.draw(size_of::<Self>(), "row walk controls")?;
        let count = Self::slot_count(config);
        let mut slots = FixedVec::new(budget, "row walk slots", count)?;
        for _ in 0..count {
            slots
                .push(Mutex::new(FixedVec::new(
                    budget,
                    "retained row walk identities",
                    Self::row_capacity(config),
                )?))
                .expect("sized to row walk slots");
        }
        Ok(Self { slots })
    }

    pub(super) fn retain(&self, rows: &RowMap) -> Result<RetainedRowIds<'_>, SqlError> {
        let mut rowids = try_mutex_pool(&self.slots, "row walk slots").ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "row walk retention slots are exhausted ({})",
                self.slots.len()
            )
        })?;
        rowids.clear();
        {
            let rows = rows.read();
            for (&rowid, _) in rows.iter() {
                rowids.push(rowid).map_err(|error| {
                    sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "{}", error)
                })?;
            }
        }
        // A sorted frozen set also owns SST suppression. Current map
        // membership can change after callbacks and cannot define coverage.
        rowids.as_mut_slice().sort_unstable();
        Ok(RetainedRowIds { rowids })
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
                storage.for_each_row_state(table, &mut |rowid, state| {
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
                        assert_eq!(bytes, &*storage.heap.get(image_location(storage, table)).unwrap());
                        Ok(())
                    })?;
                    count += 1;
                    Ok(core::ops::ControlFlow::Continue(()))
                }).unwrap();
                assert_eq!(count, 1, "a later insertion is outside the retained row set");
            });
            let undo = writer.join().unwrap();
            assert!(progressed, "writer must finish while the scan callback retains bytes");
            storage.restore_pending(table, 2, 7, undo);
        });
    }

    fn image_location(storage: &Storage, table: usize) -> super::super::RowLoc {
        match storage.tables[table].rows.get(&1).unwrap().committed.unwrap() {
            super::super::RowHome::Heap(location) => location,
            _ => panic!("fixture heap image"),
        }
    }

    #[test]
    fn retained_row_walk_survives_removal_reuse_and_nested_walks() {
        let (storage, table) = fixture();
        let location = storage.heap.append_row(&[Datum::Int4(2)]).unwrap();
        let undo = storage.write_pending(table, 2, 7, 1, Some(location)).unwrap();
        let mut seen = [0u64; 2];
        crate::mem::guard::forbid_alloc(|| {
            let mut count = 0;
            storage.for_each_row_state(table, &mut |rowid, state| {
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
            }).unwrap();
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
        let undo = storage.write_pending(table, 2, 7, 1, Some(location)).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mut count = 0;
            storage.for_each_pending_row_change(table, &mut |rowid, change| {
                assert_eq!(rowid, 2);
                assert_eq!(change.txid, 7);
                assert!(storage.tables[table].rows.test_write().is_ok());
                assert!(storage.row_versions.test_write().is_ok());
                storage.restore_pending(table, rowid, 7, undo);
                assert_eq!(change.loc, Some(location));
                assert!(storage.resident_row_state(table, rowid).is_none());
                count += 1;
                Ok(core::ops::ControlFlow::Continue(()))
            }).unwrap();
            assert_eq!(count, 1);
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
                leases.push(pool.retain(&rows).unwrap());
            }
            assert_eq!(pool.retain(&rows).unwrap_err().sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
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
        let mut budget = Budget::new(RowWalkPool::budget_bytes(&config));
        let pool = RowWalkPool::new(&config, &mut budget).unwrap();
        let mut row_budget = Budget::new(4096);
        let mut rows = RowMap::new(&mut row_budget, "oversized fixture", 2).unwrap();
        rows.insert(1, RowState::committed_only_at(super::super::RowLoc::test(0, 4), 7)).unwrap();
        rows.insert(2, RowState::committed_only_at(super::super::RowLoc::test(0, 4), 7)).unwrap();
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
