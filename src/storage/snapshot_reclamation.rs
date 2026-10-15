//! Reclaim history in bounded transitions while preserving newly admitted readers.

use std::sync::{Mutex, TryLockError};

use super::{Storage, prune_committed_history};
use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;

const ROWS_PER_BATCH: usize = 64;

pub(super) struct SnapshotReclamation {
    rowids: Mutex<FixedVec<u64>>,
}

impl SnapshotReclamation {
    fn capacity(config: &Config) -> usize {
        config.table_rows.max(config.large_object_pages)
    }

    pub(super) fn budget_bytes(config: &Config) -> usize {
        size_of::<Self>() + Self::capacity(config) * size_of::<u64>()
    }

    pub(super) fn new(config: &Config, budget: &mut Budget) -> Result<Self, BudgetError> {
        budget.draw(size_of::<Self>(), "snapshot reclamation control")?;
        Ok(Self {
            rowids: Mutex::new(FixedVec::new(
                budget,
                "snapshot reclamation row identities",
                Self::capacity(config),
            )?),
        })
    }
}

impl Storage {
    pub(super) fn prune_snapshot_versions(&self) {
        // Serialize only cleanup scratch. Callers release the registry before
        // waiting here; ordinary row reads and writes do not use this workspace.
        let mut rowids = self
            .snapshot_reclamation
            .rowids
            .lock()
            .expect("snapshot reclamation workspace poisoned");
        self.prune_snapshot_versions_in(&mut rowids);
    }

    pub(crate) fn prune_released_statement_snapshots(&self) {
        // Statement admission must not queue behind another cleanup. Its
        // pending flag remains set, so a later admission retries the work.
        let mut rowids = match self.snapshot_reclamation.rowids.try_lock() {
            Ok(rowids) => rowids,
            Err(TryLockError::WouldBlock) => return,
            Err(TryLockError::Poisoned(_)) => panic!("snapshot reclamation workspace poisoned"),
        };
        let pending = self.snapshot_state().statement_reclamation_pending;
        if pending {
            self.prune_snapshot_versions_in(&mut rowids);
        }
    }

    fn prune_snapshot_versions_in(&self, rowids: &mut FixedVec<u64>) {
        self.snapshot_state().statement_reclamation_pending = false;
        for table in 0..self.tables.len() {
            rowids.clear();
            {
                let rows = self.tables[table].rows.read();
                for (&rowid, _) in rows.iter() {
                    rowids
                        .push(rowid)
                        .expect("reclamation identities sized to every table");
                }
            }
            // Map deletion shifts backing slots. Retained identities ensure
            // neither deletion nor an intervening update skips captured rows.
            for batch in rowids.as_slice().chunks(ROWS_PER_BATCH) {
                self.prune_snapshot_row_batch(table, batch);
            }
        }
        rowids.clear();
    }

    fn prune_snapshot_row_batch(&self, table: usize, rowids: &[u64]) {
        // Match capture ordering: generations precede version and map guards.
        // Recheck retention each batch; it may change while the guards release.
        let snapshots = self.snapshot_state();
        let generations = self.tables[table].spill_generations();
        let mut versions = self.row_versions.write();
        let mut rows = self.tables[table].rows.write();
        let oldest = snapshots.oldest();
        for rowid in rowids {
            let Some(state) = rows.get_mut(rowid) else {
                continue;
            };
            prune_committed_history(
                &mut versions.committed_row_versions,
                &mut state.history,
                oldest,
            );
            if oldest.is_none()
                && generations.is_empty()
                && state.committed.is_none()
                && state.history.is_empty()
                && state.pending.is_none()
            {
                rows.remove(rowid);
            }
        }
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

    fn fixture() -> (Storage, usize, usize) {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let first = storage
            .create_table(make_def("cleanup_first", &[("id", ColType::Int4, true)]))
            .unwrap();
        let second = storage
            .create_table(make_def("cleanup_second", &[("id", ColType::Int4, true)]))
            .unwrap();
        let location = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        storage.tables[first]
            .rows
            .insert(1, RowState::committed_only_at(location, 7))
            .unwrap();
        storage.set_lsn(7);
        (storage, first, second)
    }

    #[test]
    fn snapshot_reclamation_budget_matches_largest_table_and_rejects_short_startup() {
        let mut config = test_config();
        for (rows, pages) in [(16, 8), (8, 16)] {
            config.table_rows = rows;
            config.large_object_pages = pages;
            let bytes = SnapshotReclamation::budget_bytes(&config);
            let mut budget = Budget::new(bytes);
            let workspace = SnapshotReclamation::new(&config, &mut budget).unwrap();
            assert_eq!(budget.used(), bytes);
            assert_eq!(workspace.rowids.lock().unwrap().capacity(), 16);
            let mut short = Budget::new(bytes - 1);
            let error = SnapshotReclamation::new(&config, &mut short).err().unwrap();
            assert_eq!(error.what, "snapshot reclamation row identities");
            assert_eq!(error.requested, 16 * size_of::<u64>());
        }
    }

    #[test]
    fn snapshot_reclamation_allows_unrelated_publication_while_another_table_waits() {
        for mode in 0..3 {
            let (mut storage, first, second) = fixture();
            let reader = if mode == 2 {
                Some(storage.retain_statement_snapshot(99, |lsn| lsn).unwrap())
            } else {
                storage.register_snapshot(99, 7).unwrap();
                None
            };
            let location = storage.heap.append_row(&[Datum::Int4(10)]).unwrap();
            storage
                .write_pending(first, 1, 8, 1, Some(location))
                .unwrap();
            storage.commit_row(first, 1, 8, 9).unwrap();
            storage.set_lsn(9);
            assert!(
                !storage.tables[first]
                    .rows
                    .get(&1)
                    .unwrap()
                    .history
                    .is_empty()
            );
            let blocked_table = storage.tables[second].rows.write();
            let completed = AtomicBool::new(false);
            let mut progressed = false;
            let mut reached_second = false;
            std::thread::scope(|scope| {
                let cleanup_storage = &storage;
                let cleanup = scope.spawn(move || {
                    crate::mem::guard::forbid_alloc(|| match mode {
                        0 => cleanup_storage.release_snapshot(99),
                        1 => cleanup_storage.end_transaction_snapshots(99),
                        _ => {
                            drop(reader);
                            cleanup_storage.prune_released_statement_snapshots();
                        }
                    })
                });
                crate::mem::guard::forbid_alloc(|| {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while Instant::now() < deadline {
                        if storage.tables[first]
                            .rows
                            .get(&1)
                            .unwrap()
                            .history
                            .is_empty()
                        {
                            reached_second = true;
                            break;
                        }
                        std::thread::yield_now();
                    }
                });
                let writer_storage = &storage;
                let completed = &completed;
                let writer = scope.spawn(move || {
                    crate::mem::guard::forbid_alloc(|| {
                        let ephemeral = writer_storage
                            .retain_statement_snapshot(101, |lsn| lsn)
                            .unwrap();
                        drop(ephemeral);
                        let reader = writer_storage
                            .retain_statement_snapshot(100, |lsn| lsn)
                            .unwrap();
                        assert_eq!(reader.snapshot(), 9);
                        writer_storage.register_snapshot(100, 9).unwrap();
                        let location = writer_storage.heap.append_row(&[Datum::Int4(20)]).unwrap();
                        writer_storage
                            .write_pending(first, 1, 10, 1, Some(location))
                            .unwrap();
                        writer_storage.commit_row(first, 1, 10, 11).unwrap();
                        completed.store(true, Ordering::Release);
                    })
                });
                crate::mem::guard::forbid_alloc(|| {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while !completed.load(Ordering::Acquire) && Instant::now() < deadline {
                        std::thread::yield_now();
                    }
                    progressed = completed.load(Ordering::Acquire);
                });
                // Always unblock cleanup before joining, including a failing regression.
                drop(blocked_table);
                cleanup.join().unwrap();
                writer.join().unwrap();
            });
            assert!(reached_second, "cleanup must finish the first table");
            assert!(
                progressed,
                "cleanup waiting on one table must release shared registry and versions"
            );
            crate::mem::guard::forbid_alloc(|| {
                let state = storage.resident_row_state(first, 1).unwrap().unwrap();
                let image = storage
                    .visible_row_home_at(first, 1, state, 100, SNAPSHOT_ALL, 9)
                    .unwrap()
                    .unwrap();
                storage
                    .with_row_bytes(first, 1, image, |bytes| {
                        let mut values = [Datum::Null];
                        crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                        assert_eq!(values[0], Datum::Int4(10));
                        Ok(())
                    })
                    .unwrap();
                storage.release_snapshot(100);
                assert!(
                    storage
                        .snapshot_reclamation
                        .rowids
                        .lock()
                        .unwrap()
                        .is_empty()
                );
            });
        }
    }

    #[test]
    fn snapshot_reclamation_rechecks_readers_and_preserves_already_issued_bytes() {
        let (storage, table, _) = fixture();
        crate::mem::guard::forbid_alloc(|| {
            storage.register_snapshot(99, 7).unwrap();
            let location = storage.heap.append_row(&[Datum::Int4(20)]).unwrap();
            storage
                .write_pending(table, 1, 8, 1, Some(location))
                .unwrap();
            storage.commit_row(table, 1, 8, 9).unwrap();
            let state = storage.resident_row_state(table, 1).unwrap().unwrap();
            storage.prune_snapshot_row_batch(table, &[1]);
            assert_eq!(
                storage
                    .resident_row_state(table, 1)
                    .unwrap()
                    .unwrap()
                    .history
                    .len(),
                1
            );
            storage.release_snapshot(99);
            assert!(
                storage
                    .resident_row_state(table, 1)
                    .unwrap()
                    .unwrap()
                    .history
                    .is_empty()
            );
            let image = storage
                .visible_row_home_at(table, 1, state, 99, SNAPSHOT_ALL, 7)
                .unwrap()
                .unwrap();
            storage
                .with_row_bytes(table, 1, image, |bytes| {
                    let mut values = [Datum::Null];
                    crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                    assert_eq!(values[0], Datum::Int4(1));
                    Ok(())
                })
                .unwrap();
        });
    }

    #[test]
    fn snapshot_reclamation_removes_every_deleted_row_across_shifted_map_slots() {
        let mut config = test_config();
        config.table_rows = 1024;
        config.large_object_pages = 2048;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let table = storage
            .create_table(make_def(
                "cleanup_deletions",
                &[("id", ColType::Int4, true)],
            ))
            .unwrap();
        let pages = storage.large_object_page_table as usize;
        for (slot, count) in [
            (table, config.table_rows),
            (pages, config.large_object_pages),
        ] {
            for rowid in 1..=count as u64 {
                let mut state = RowState::committed_only_at(crate::storage::RowLoc::test(0, 0), 7);
                state.committed = None;
                storage.tables[slot].rows.insert(rowid, state).unwrap();
            }
        }
        crate::mem::guard::forbid_alloc(|| {
            storage.prune_snapshot_versions();
            assert!(storage.tables[table].rows.is_empty());
            assert!(storage.tables[pages].rows.is_empty());
            storage.prune_snapshot_versions();
            assert!(
                storage
                    .snapshot_reclamation
                    .rowids
                    .lock()
                    .unwrap()
                    .is_empty()
            );
        });
    }

    #[test]
    fn snapshot_reclamation_does_not_consume_query_row_walk_slots() {
        let (storage, table, _) = fixture();
        let mut walks = Vec::with_capacity(crate::storage::MAX_ROW_WALK_NESTING);
        crate::mem::guard::forbid_alloc(|| {
            for _ in 0..crate::storage::MAX_ROW_WALK_NESTING {
                walks.push(
                    storage
                        .row_walks
                        .retain(&storage.tables[table].rows)
                        .unwrap(),
                );
            }
            storage.register_snapshot(99, 7).unwrap();
            let location = storage.heap.append_row(&[Datum::Int4(20)]).unwrap();
            storage
                .write_pending(table, 1, 8, 1, Some(location))
                .unwrap();
            storage.commit_row(table, 1, 8, 9).unwrap();
            storage.release_snapshot(99);
            assert!(
                storage
                    .resident_row_state(table, 1)
                    .unwrap()
                    .unwrap()
                    .history
                    .is_empty()
            );
            walks.clear();
        });
    }
}
