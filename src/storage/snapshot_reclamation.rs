//! Reclaim history in bounded transitions while preserving newly admitted readers.

use std::sync::{Mutex, TryLockError};

use super::{Storage, prune_committed_history};
use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;

const ROWS_PER_BATCH: usize = 64;

pub(super) struct SnapshotReclamation {
    workspace: Mutex<ReclamationWorkspace>,
}

struct ReclamationWorkspace {
    rowids: FixedVec<u64>,
    retry: bool,
}

#[derive(Clone, Copy)]
enum ReclamationMode {
    Wait,
    SkipBusy,
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
            workspace: Mutex::new(ReclamationWorkspace {
                rowids: FixedVec::new(
                    budget,
                    "snapshot reclamation row identities",
                    Self::capacity(config),
                )?,
                retry: false,
            }),
        })
    }
}

struct SnapshotRowOwners<'storage> {
    snapshots: std::sync::MutexGuard<'storage, super::SnapshotState>,
    generations:
        std::sync::RwLockReadGuard<'storage, super::spill_generations::SpillGenerationList>,
    versions: std::sync::RwLockWriteGuard<'storage, super::row_versions::RowVersionState>,
    rows: std::sync::RwLockWriteGuard<
        'storage,
        crate::mem::fixed_map::FixedMap<u64, super::RowState>,
    >,
}

impl Storage {
    pub(super) fn prune_snapshot_versions(&self) {
        // Explicit release completes reclamation; statement admission only tries it.
        let mut workspace = self
            .snapshot_reclamation
            .workspace
            .lock()
            .expect("snapshot reclamation workspace poisoned");
        self.snapshot_state().statement_reclamation_pending = false;
        workspace.retry =
            !self.prune_snapshot_versions_in(&mut workspace.rowids, ReclamationMode::Wait);
    }

    pub(crate) fn prune_released_statement_snapshots(&self) {
        let mut workspace = match self.snapshot_reclamation.workspace.try_lock() {
            Ok(workspace) => workspace,
            Err(TryLockError::WouldBlock) => return,
            Err(TryLockError::Poisoned(_)) => panic!("snapshot reclamation workspace poisoned"),
        };
        let Ok(mut snapshots) =
            super::row_ownership::try_mutex(&self.snapshots, "snapshot state lock poisoned")
        else {
            return;
        };
        if !snapshots.statement_reclamation_pending && !workspace.retry {
            return;
        }
        // Consume only this request. A release during the pass sets a new one;
        // skipped owners have a separate retry bit in this startup workspace.
        snapshots.statement_reclamation_pending = false;
        drop(snapshots);
        workspace.retry =
            !self.prune_snapshot_versions_in(&mut workspace.rowids, ReclamationMode::SkipBusy);
    }

    fn prune_snapshot_versions_in(
        &self,
        rowids: &mut FixedVec<u64>,
        mode: ReclamationMode,
    ) -> bool {
        let mut complete = true;
        for table in 0..self.tables.len() {
            rowids.clear();
            {
                let rows = match mode {
                    ReclamationMode::Wait => self.tables[table].rows.read(),
                    ReclamationMode::SkipBusy => {
                        let Ok(rows) = self.tables[table].rows.try_read_for_batch() else {
                            complete = false;
                            continue;
                        };
                        rows
                    }
                };
                for (&rowid, _) in rows.iter() {
                    rowids
                        .push(rowid)
                        .expect("reclamation identities sized to every table");
                }
            }
            // Map deletion shifts backing slots. Retained identities ensure
            // neither deletion nor an intervening update skips captured rows.
            for batch in rowids.as_slice().chunks(ROWS_PER_BATCH) {
                if !self.prune_snapshot_row_batch(table, batch, mode) {
                    complete = false;
                    break;
                }
            }
        }
        rowids.clear();
        complete
    }

    fn try_snapshot_row_owners(
        &self,
        table: usize,
    ) -> Result<SnapshotRowOwners<'_>, super::row_ownership::LockWait<'_>> {
        Ok(SnapshotRowOwners {
            snapshots: super::row_ownership::try_mutex(
                &self.snapshots,
                "snapshot state lock poisoned",
            )?,
            generations: self.tables[table].spill_generations.try_read_for_batch()?,
            versions: self.row_versions.try_write_for_batch()?,
            rows: self.tables[table].rows.try_write_for_batch()?,
        })
    }

    fn prune_snapshot_row_batch(
        &self,
        table: usize,
        rowids: &[u64],
        mode: ReclamationMode,
    ) -> bool {
        // Recheck retention after every wait and between bounded row batches.
        let SnapshotRowOwners {
            snapshots,
            generations,
            mut versions,
            mut rows,
        } = match mode {
            ReclamationMode::Wait => {
                super::row_ownership::acquire(|| self.try_snapshot_row_owners(table))
            }
            ReclamationMode::SkipBusy => {
                let Ok(owners) = self.try_snapshot_row_owners(table) else {
                    return false;
                };
                owners
            }
        };
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
        true
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
    fn row_lock_owners_snapshot_cleanup_releases_globals_on_a_busy_row_map() {
        let (storage, first, second) = fixture();
        crate::mem::guard::forbid_alloc(|| {
            storage.register_snapshot(99, 7).unwrap();
            let image = storage.heap.append_row_image(&[Datum::Int4(20)]).unwrap();
            storage
                .write_pending_image_untracked(first, 1, 8, 1, Some(image))
                .unwrap();
            storage.commit_row(first, 1, 8, 9).unwrap();
            storage.observe_lsn(9);
            let selected = storage.resident_row_state(first, 1).unwrap().unwrap();
            let retained = storage
                .visible_row_home_at(first, 1, selected, 99, SNAPSHOT_ALL, 7)
                .unwrap()
                .unwrap();

            let held = storage.tables[first].rows.write();
            assert_eq!(held.get(&1).unwrap().history.len(), 1);
            // Force the pruning stage itself to contend, after it attempted
            // registry and version ownership, rather than blocking capture.
            let wait = storage.try_snapshot_row_owners(first).err().unwrap();
            assert!(storage.snapshots.try_lock().is_ok());
            assert!(storage.row_versions.test_write().is_ok());

            let image = storage.heap.append_row_image(&[Datum::Int4(30)]).unwrap();
            let undo = storage
                .write_pending_image_untracked(second, 1, 10, 1, Some(image))
                .unwrap();
            storage.restore_pending(second, 1, 10, undo);
            assert!(storage.tables[second].rows.is_empty());
            drop(wait);
            drop(held);

            assert!(storage.prune_snapshot_row_batch(first, &[1], ReclamationMode::Wait));
            assert_eq!(storage.tables[first].rows.get(&1).unwrap().history.len(), 1);
            storage.release_snapshot(99);
            assert!(
                storage.tables[first]
                    .rows
                    .get(&1)
                    .unwrap()
                    .history
                    .is_empty()
            );
            storage
                .with_row_bytes(first, 1, retained, |bytes| {
                    let mut values = [Datum::Null];
                    crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                    assert_eq!(values[0], Datum::Int4(1));
                    Ok(())
                })
                .unwrap();
        });
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
            let state = workspace.workspace.lock().unwrap();
            assert_eq!(state.rowids.capacity(), 16);
            assert!(!state.retry);
            drop(state);
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
                        .workspace
                        .lock()
                        .unwrap()
                        .rowids
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
            assert!(storage.prune_snapshot_row_batch(table, &[1], ReclamationMode::Wait));
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
                    .workspace
                    .lock()
                    .unwrap()
                    .rowids
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

    #[test]
    fn nonblocking_snapshot_admission_skips_busy_rows_and_retries_without_losing_images() {
        let (mut storage, first, second) = fixture();
        let location = storage.heap.append_row(&[Datum::Int4(2)]).unwrap();
        storage.tables[second]
            .rows
            .insert(1, RowState::committed_only_at(location, 7))
            .unwrap();
        let old_reader = storage.retain_statement_snapshot(99, |lsn| lsn).unwrap();
        let selected = storage.resident_row_state(first, 1).unwrap().unwrap();
        let image = storage
            .visible_row_home_at(first, 1, selected, 99, SNAPSHOT_ALL, 7)
            .unwrap()
            .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            for (table, txid, lsn, value) in [(first, 8, 9, 20), (second, 10, 11, 30)] {
                let image = storage
                    .heap
                    .append_row_image(&[Datum::Int4(value)])
                    .unwrap();
                storage
                    .write_pending_image_untracked(table, 1, txid, 1, Some(image))
                    .unwrap();
                storage.commit_row(table, 1, txid, lsn).unwrap();
                storage.observe_lsn(lsn);
            }
            drop(old_reader);
        });
        let held = storage.tables[first].rows.write();
        let finished = AtomicBool::new(false);
        let (progressed, reader) = std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                let reader = crate::mem::guard::forbid_alloc(|| {
                    storage.retain_statement_snapshot(101, |lsn| lsn).unwrap()
                });
                finished.store(true, Ordering::Release);
                reader
            });
            let progressed = crate::mem::guard::forbid_alloc(|| {
                let deadline = Instant::now() + Duration::from_secs(5);
                while !finished.load(Ordering::Acquire) && Instant::now() < deadline {
                    std::thread::yield_now();
                }
                finished.load(Ordering::Acquire)
            });
            assert_eq!(held.get(&1).unwrap().history.len(), 1);
            // Unblock even a regression before joining the worker.
            drop(held);
            (progressed, worker.join().unwrap())
        });
        assert!(
            progressed,
            "statement admission waited for another table's cleanup"
        );
        assert_eq!(reader.snapshot(), 11);
        crate::mem::guard::forbid_alloc(|| {
            assert!(
                storage.tables[second]
                    .rows
                    .get(&1)
                    .unwrap()
                    .history
                    .is_empty()
            );
            assert!(storage.snapshot_reclamation.workspace.lock().unwrap().retry);
            assert!(!storage.snapshot_state().statement_reclamation_pending);
            storage.prune_released_statement_snapshots();
            assert_eq!(storage.tables[first].rows.get(&1).unwrap().history.len(), 1);
            let workspace = storage.snapshot_reclamation.workspace.lock().unwrap();
            assert!(!workspace.retry);
            assert!(workspace.rowids.is_empty());
            drop(workspace);
            drop(reader);
            assert!(storage.snapshot_state().statement_reclamation_pending);
            storage.prune_released_statement_snapshots();
            assert!(
                storage.tables[first]
                    .rows
                    .get(&1)
                    .unwrap()
                    .history
                    .is_empty()
            );
            assert!(!storage.snapshot_state().statement_reclamation_pending);
            storage
                .with_row_bytes(first, 1, image, |bytes| {
                    let mut values = [Datum::Null];
                    crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                    assert_eq!(values[0], Datum::Int4(1));
                    Ok(())
                })
                .unwrap();
        });
    }

    #[test]
    fn nonblocking_snapshot_cleanup_releases_owners_and_rechecks_readers_on_retry() {
        let (storage, table, _) = fixture();
        let reader = storage.retain_statement_snapshot(99, |lsn| lsn).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let image = storage.heap.append_row_image(&[Datum::Int4(20)]).unwrap();
            storage
                .write_pending_image_untracked(table, 1, 8, 1, Some(image))
                .unwrap();
            storage.commit_row(table, 1, 8, 9).unwrap();
            storage.observe_lsn(9);
            drop(reader);
        });
        let versions = storage.row_versions.test_write().unwrap();
        let finished = AtomicBool::new(false);
        let progressed = std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                crate::mem::guard::forbid_alloc(|| storage.prune_released_statement_snapshots());
                finished.store(true, Ordering::Release);
            });
            let progressed = crate::mem::guard::forbid_alloc(|| {
                let deadline = Instant::now() + Duration::from_secs(5);
                while !finished.load(Ordering::Acquire) && Instant::now() < deadline {
                    std::thread::yield_now();
                }
                finished.load(Ordering::Acquire)
            });
            // Check guards only after the worker returned, without waiting on them.
            let registry_released = storage.snapshots.try_lock().is_ok();
            let generations_released = storage.tables[table]
                .spill_generations
                .try_read_for_batch()
                .is_ok();
            let rows_released = storage.tables[table].rows.test_write().is_ok();
            drop(versions);
            worker.join().unwrap();
            assert!(registry_released && generations_released && rows_released);
            progressed
        });
        assert!(
            progressed,
            "opportunistic cleanup waited for row-version ownership"
        );
        crate::mem::guard::forbid_alloc(|| {
            assert!(storage.snapshot_reclamation.workspace.lock().unwrap().retry);
            assert_eq!(storage.tables[table].rows.get(&1).unwrap().history.len(), 1);
            storage.register_snapshot(100, 7).unwrap();
            storage.prune_released_statement_snapshots();
            assert!(!storage.snapshot_reclamation.workspace.lock().unwrap().retry);
            assert_eq!(storage.tables[table].rows.get(&1).unwrap().history.len(), 1);
            storage.release_snapshot(100);
            assert!(
                storage.tables[table]
                    .rows
                    .get(&1)
                    .unwrap()
                    .history
                    .is_empty()
            );
        });
    }

    #[test]
    fn nonblocking_snapshot_cleanup_skips_busy_control_owners_without_consuming_work() {
        let (storage, _, _) = fixture();
        for busy_registry in [true, false] {
            let reader = storage.retain_statement_snapshot(99, |lsn| lsn).unwrap();
            crate::mem::guard::forbid_alloc(|| drop(reader));
            let registry = busy_registry.then(|| storage.snapshot_state());
            let workspace =
                (!busy_registry).then(|| storage.snapshot_reclamation.workspace.lock().unwrap());
            let finished = AtomicBool::new(false);
            let progressed = std::thread::scope(|scope| {
                let worker = scope.spawn(|| {
                    crate::mem::guard::forbid_alloc(|| {
                        storage.prune_released_statement_snapshots()
                    });
                    finished.store(true, Ordering::Release);
                });
                let progressed = crate::mem::guard::forbid_alloc(|| {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while !finished.load(Ordering::Acquire) && Instant::now() < deadline {
                        std::thread::yield_now();
                    }
                    finished.load(Ordering::Acquire)
                });
                drop(registry);
                drop(workspace);
                worker.join().unwrap();
                progressed
            });
            assert!(
                progressed,
                "opportunistic cleanup waited for a control owner"
            );
            crate::mem::guard::forbid_alloc(|| {
                assert!(storage.snapshot_state().statement_reclamation_pending);
                storage.prune_released_statement_snapshots();
                assert!(!storage.snapshot_state().statement_reclamation_pending);
            });
        }
    }

    #[test]
    fn nonblocking_snapshot_batch_releases_prefix_owners_when_rows_are_busy() {
        let (storage, table, _) = fixture();
        crate::mem::guard::forbid_alloc(|| {
            let rows = storage.tables[table].rows.write();
            assert!(!storage.prune_snapshot_row_batch(table, &[1], ReclamationMode::SkipBusy));
            assert!(storage.snapshots.try_lock().is_ok());
            assert!(storage.row_versions.test_write().is_ok());
            assert!(
                storage.tables[table]
                    .spill_generations
                    .try_read_for_batch()
                    .is_ok()
            );
            assert!(rows.get(&1).is_some());
        });
    }

    #[test]
    fn nonblocking_snapshot_release_racing_cleanup_keeps_its_reclamation_request() {
        let (storage, table, _) = fixture();
        for iteration in 0..8 {
            let reader = storage.retain_statement_snapshot(99, |lsn| lsn).unwrap();
            crate::mem::guard::forbid_alloc(|| {
                let released = storage.retain_statement_snapshot(100, |lsn| lsn).unwrap();
                drop(released);
                assert!(storage.snapshot_state().statement_reclamation_pending);
                let image = storage
                    .heap
                    .append_row_image(&[Datum::Int4(20 + iteration)])
                    .unwrap();
                let lsn = 9 + iteration as u64 * 2;
                storage
                    .write_pending_image_untracked(table, 1, 8, 1, Some(image))
                    .unwrap();
                storage.commit_row(table, 1, 8, lsn).unwrap();
                storage.observe_lsn(lsn);
            });
            let start = std::sync::Barrier::new(2);
            std::thread::scope(|scope| {
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    crate::mem::guard::forbid_alloc(|| drop(reader));
                });
                start.wait();
                crate::mem::guard::forbid_alloc(|| storage.prune_released_statement_snapshots());
            });
            crate::mem::guard::forbid_alloc(|| {
                if !storage.tables[table]
                    .rows
                    .get(&1)
                    .unwrap()
                    .history
                    .is_empty()
                {
                    assert!(
                        storage.snapshot_state().statement_reclamation_pending
                            || storage.snapshot_reclamation.workspace.lock().unwrap().retry
                    );
                }
                storage.prune_released_statement_snapshots();
                assert!(
                    storage.tables[table]
                        .rows
                        .get(&1)
                        .unwrap()
                        .history
                        .is_empty()
                );
                assert!(!storage.snapshot_state().statement_reclamation_pending);
                assert!(!storage.snapshot_reclamation.workspace.lock().unwrap().retry);
            });
        }
    }
}
