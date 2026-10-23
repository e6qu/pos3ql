//! Bound history maintenance by stable row slots; active readers keep their versions.

use core::num::NonZeroUsize;
use {crate::mem::mutex::Mutex, std::sync::TryLockError};

use super::{Storage, prune_committed_history};
use crate::mem::budget::{Budget, BudgetError};

const SLOTS_PER_BATCH: NonZeroUsize = NonZeroUsize::new(64).unwrap();

pub(super) struct SnapshotReclamation {
    workspace: Mutex<ReclamationWorkspace>,
}

struct ReclamationWorkspace {
    cursor: Option<ReclamationCursor>,
    retry: bool,
}

#[derive(Clone, Copy)]
struct ReclamationCursor {
    table: usize,
    slot: usize,
    pressure_started: bool,
    pressure_found: bool,
}

#[derive(Clone, Copy)]
enum ReclamationMode {
    Wait,
    SkipBusy,
}

#[derive(Clone, Copy)]
enum PressureScan {
    Start,
    Continue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SnapshotCleanup {
    Schedule,
    Finish,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HistoryReclamation {
    Idle,
    Advanced,
    WaitingForOwner,
}

struct PrunedSlots {
    next: usize,
    visited: usize,
    complete: bool,
    pressure: bool,
}

impl SnapshotReclamation {
    pub(super) fn budget_bytes() -> usize {
        size_of::<Self>()
    }

    pub(super) fn new(budget: &mut Budget) -> Result<Self, BudgetError> {
        budget.draw(size_of::<Self>(), "snapshot reclamation control")?;
        Ok(Self {
            workspace: Mutex::new(ReclamationWorkspace {
                cursor: None,
                retry: false,
            }),
        })
    }
}

struct SnapshotRowOwners<'storage> {
    snapshots: crate::mem::mutex::MutexGuard<'storage, super::SnapshotState>,
    generations:
        std::sync::RwLockReadGuard<'storage, super::spill_generations::SpillGenerationList>,
    versions: std::sync::RwLockWriteGuard<'storage, super::row_versions::RowVersionState>,
    rows: std::sync::RwLockWriteGuard<'storage, super::RowEntries>,
}

impl Storage {
    pub(super) fn prune_snapshot_versions(&self) {
        // Explicit completion drains cleanup. Admission never owns this workspace.
        let mut workspace = self
            .snapshot_reclamation
            .workspace
            .lock()
            .expect("snapshot reclamation workspace poisoned");
        self.snapshot_state().statement_reclamation_pending = false;
        workspace.cursor = None;
        workspace.retry = false;
        let mut pressure_started = false;
        let mut pressure_found = false;
        for table in 0..self.tables.len() {
            let mut slot = 0;
            loop {
                let batch = self
                    .prune_snapshot_row_slots(
                        table,
                        slot,
                        SLOTS_PER_BATCH,
                        ReclamationMode::Wait,
                        if pressure_started {
                            PressureScan::Continue
                        } else {
                            PressureScan::Start
                        },
                    )
                    .expect("waiting cleanup acquires its owners");
                pressure_started = true;
                pressure_found |= batch.pressure;
                if batch.complete {
                    break;
                }
                slot = batch.next;
            }
        }
        assert!(self.finish_history_pressure(pressure_found, ReclamationMode::Wait));
    }

    pub(crate) fn history_cleanup_pending(&self) -> bool {
        let workspace = match self.snapshot_reclamation.workspace.try_lock() {
            Ok(workspace) => workspace,
            Err(TryLockError::WouldBlock) => return true,
            Err(TryLockError::Poisoned(_)) => panic!("snapshot reclamation workspace poisoned"),
        };
        if workspace.cursor.is_some() || workspace.retry {
            return true;
        }
        let Ok(snapshots) =
            super::row_ownership::try_mutex(&self.snapshots, "snapshot state lock poisoned")
        else {
            return true;
        };
        snapshots.statement_reclamation_pending
    }

    pub(crate) fn advance_history_cleanup(&self, work: NonZeroUsize) -> HistoryReclamation {
        let mut workspace = match self.snapshot_reclamation.workspace.try_lock() {
            Ok(workspace) => workspace,
            Err(TryLockError::WouldBlock) => return HistoryReclamation::WaitingForOwner,
            Err(TryLockError::Poisoned(_)) => panic!("snapshot reclamation workspace poisoned"),
        };
        if workspace.cursor.is_none() {
            let Ok(mut snapshots) =
                super::row_ownership::try_mutex(&self.snapshots, "snapshot state lock poisoned")
            else {
                return HistoryReclamation::WaitingForOwner;
            };
            if !snapshots.statement_reclamation_pending && !workspace.retry {
                return HistoryReclamation::Idle;
            }
            // Releases arriving during the pass remain queued for another pass.
            snapshots.statement_reclamation_pending = false;
            workspace.retry = false;
            workspace.cursor = Some(ReclamationCursor {
                table: 0,
                slot: 0,
                pressure_started: false,
                pressure_found: false,
            });
        }
        let mut cursor = workspace.cursor.expect("cleanup pass started");
        let mut remaining = work.get();
        while remaining != 0 && cursor.table < self.tables.len() {
            let limit = NonZeroUsize::new(remaining).expect("remaining work is nonzero");
            match self.prune_snapshot_row_slots(
                cursor.table,
                cursor.slot,
                limit,
                ReclamationMode::SkipBusy,
                if cursor.pressure_started {
                    PressureScan::Continue
                } else {
                    PressureScan::Start
                },
            ) {
                Some(batch) => {
                    cursor.pressure_started = true;
                    cursor.pressure_found |= batch.pressure;
                    remaining -= batch.visited.max(1);
                    if batch.complete {
                        cursor.table += 1;
                        cursor.slot = 0;
                    } else {
                        cursor.slot = batch.next;
                    }
                }
                None => {
                    workspace.retry = true;
                    cursor.table += 1;
                    cursor.slot = 0;
                    remaining -= 1;
                }
            }
        }
        if cursor.table == self.tables.len() {
            if workspace.retry {
                workspace.cursor = None;
                return HistoryReclamation::WaitingForOwner;
            }
            if !self.finish_history_pressure(cursor.pressure_found, ReclamationMode::SkipBusy) {
                workspace.cursor = Some(cursor);
                return HistoryReclamation::WaitingForOwner;
            }
            workspace.cursor = None;
            HistoryReclamation::Advanced
        } else {
            workspace.cursor = Some(cursor);
            HistoryReclamation::Advanced
        }
    }

    fn finish_history_pressure(&self, found: bool, mode: ReclamationMode) -> bool {
        let versions = match mode {
            ReclamationMode::Wait => {
                super::row_ownership::acquire(|| self.row_versions.try_write_for_batch())
            }
            ReclamationMode::SkipBusy => {
                let Ok(versions) = self.row_versions.try_write_for_batch() else {
                    return false;
                };
                versions
            }
        };
        // A publisher after the pass began may have changed an earlier row.
        // The version owner prevents clearing its signal during finalization.
        if found || !versions.history_pressure_changed {
            self.row_versions
                .history_pressure
                .store(found, std::sync::atomic::Ordering::Release);
        }
        true
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

    fn prune_snapshot_row_slots(
        &self,
        table: usize,
        start: usize,
        work: NonZeroUsize,
        mode: ReclamationMode,
        pressure_scan: PressureScan,
    ) -> Option<PrunedSlots> {
        let SnapshotRowOwners {
            snapshots,
            generations,
            mut versions,
            mut rows,
        } = match mode {
            ReclamationMode::Wait => {
                super::row_ownership::acquire(|| self.try_snapshot_row_owners(table))
            }
            ReclamationMode::SkipBusy => self.try_snapshot_row_owners(table).ok()?,
        };
        if matches!(pressure_scan, PressureScan::Start) {
            versions.history_pressure_changed = false;
        }
        assert!(
            start <= rows.scan_end(),
            "cleanup cursor exceeds used row slots"
        );
        if rows.is_empty() {
            return Some(PrunedSlots {
                next: rows.scan_end(),
                visited: 0,
                complete: true,
                pressure: false,
            });
        }
        let end = start.saturating_add(work.get()).min(rows.scan_end());
        let oldest = snapshots.oldest();
        let mut pressure = false;
        for slot in start..end {
            let remove = if let Some((rowid, state)) = rows.entry_at_slot_mut(slot) {
                prune_committed_history(
                    &mut versions.committed_row_versions,
                    &mut state.history,
                    oldest,
                );
                pressure |= !state.history.is_empty()
                    && state.history.len().saturating_add(2) >= self.max_row_versions_per_row;
                (oldest.is_none()
                    && generations.is_empty()
                    && state.committed.is_none()
                    && state.history.is_empty()
                    && state.pending.is_none())
                .then_some(*rowid)
            } else {
                None
            };
            if let Some(rowid) = remove {
                rows.remove(&rowid).expect("selected row exists");
            }
        }
        Some(PrunedSlots {
            next: end,
            visited: end - start,
            complete: end == rows.scan_end(),
            pressure,
        })
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

    fn finish_history_work(storage: &Storage) {
        let reserved: usize = storage
            .tables
            .iter()
            .map(|table| table.rows.capacity() + 1)
            .sum();
        for _ in 0..=reserved * 2 + 1 {
            if !storage.history_cleanup_pending() {
                return;
            }
            assert_ne!(
                storage.advance_history_cleanup(SLOTS_PER_BATCH),
                HistoryReclamation::WaitingForOwner
            );
        }
        panic!("history cleanup did not finish within its reserved slot bound");
    }

    fn finish_blocked_pass(storage: &Storage) {
        // These fixtures have one occupied slot per table. A busy table is
        // skipped, so at most one turn per physical table completes the pass.
        for _ in 0..=storage.tables.len() {
            if storage.advance_history_cleanup(SLOTS_PER_BATCH)
                == HistoryReclamation::WaitingForOwner
            {
                return;
            }
        }
        panic!("busy-owner cleanup did not retain its retry request");
    }

    #[test]
    fn row_lock_owners_snapshot_cleanup_releases_globals_on_a_busy_row_map() {
        let (storage, first, second) = fixture();
        crate::mem::guard::forbid_alloc(|| {
            storage.register_snapshot(99, 7).unwrap();
            let image = storage.heap.append_row_image(&[Datum::Int4(20)]).unwrap();
            storage
                .write_pending_image(storage.select_table_definition(first, 8).0, 1, 1, Some(image), crate::storage::RowWriteStatistics::Untracked)
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
            let _wait = storage.try_snapshot_row_owners(first).err().unwrap();
            assert!(storage.snapshots.try_lock().is_ok());
            assert!(storage.row_versions.test_write().is_ok());

            let image = storage.heap.append_row_image(&[Datum::Int4(30)]).unwrap();
            let undo = storage
                .write_pending_image(storage.select_table_definition(second, 10).0, 1, 1, Some(image), crate::storage::RowWriteStatistics::Untracked)
                .unwrap();
            storage.restore_pending(second, 1, 10, undo);
            assert!(storage.tables[second].rows.is_empty());
            drop(held);

            assert!(
                storage
                    .prune_snapshot_row_slots(
                        first,
                        0,
                        SLOTS_PER_BATCH,
                        ReclamationMode::Wait,
                        PressureScan::Continue
                    )
                    .is_some()
            );
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
    fn bounded_snapshot_control_budget_is_fixed_and_rejects_short_startup() {
        let bytes = SnapshotReclamation::budget_bytes();
        let mut budget = Budget::new(bytes);
        let control = SnapshotReclamation::new(&mut budget).unwrap();
        assert_eq!(budget.remaining(), 0);
        let state = control.workspace.lock().unwrap();
        assert!(state.cursor.is_none());
        assert!(!state.retry);
        drop(state);
        let mut short = Budget::new(bytes - 1);
        let error = SnapshotReclamation::new(&mut short).err().unwrap();
        assert_eq!(error.what, "snapshot reclamation control");
        assert_eq!(error.requested, bytes);
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
                        1 => cleanup_storage
                            .end_transaction_snapshots(99, crate::storage::SnapshotCleanup::Finish),
                        _ => {
                            drop(reader);
                            cleanup_storage.prune_snapshot_versions();
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
                        .cursor
                        .is_none()
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
            assert!(
                storage
                    .prune_snapshot_row_slots(
                        table,
                        0,
                        SLOTS_PER_BATCH,
                        ReclamationMode::Wait,
                        PressureScan::Continue
                    )
                    .is_some()
            );
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
    fn snapshot_reclamation_removes_every_deleted_row_across_stable_slots() {
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
                    .cursor
                    .is_none()
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
                    .write_pending_image(storage.select_table_definition(table, txid).0, 1, 1, Some(image), crate::storage::RowWriteStatistics::Untracked)
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
            drop(held);
            (progressed, worker.join().unwrap())
        });
        assert!(
            progressed,
            "statement admission waited for another table's cleanup"
        );
        assert_eq!(reader.snapshot(), 11);
        crate::mem::guard::forbid_alloc(|| {
            // Admission does no maintenance, including on an available table.
            assert_eq!(
                storage.tables[second].rows.get(&1).unwrap().history.len(),
                1
            );
            assert!(storage.snapshot_state().statement_reclamation_pending);
            assert!(
                storage
                    .snapshot_reclamation
                    .workspace
                    .lock()
                    .unwrap()
                    .cursor
                    .is_none()
            );
            drop(reader);
            let held = storage.tables[first].rows.write();
            finish_blocked_pass(&storage);
            assert!(
                storage.tables[second]
                    .rows
                    .get(&1)
                    .unwrap()
                    .history
                    .is_empty()
            );
            assert_eq!(held.get(&1).unwrap().history.len(), 1);
            assert!(storage.snapshot_reclamation.workspace.lock().unwrap().retry);
            assert!(!storage.snapshot_state().statement_reclamation_pending);
            drop(held);
            finish_history_work(&storage);
            assert!(
                storage.tables[first]
                    .rows
                    .get(&1)
                    .unwrap()
                    .history
                    .is_empty()
            );
            assert!(!storage.history_cleanup_pending());
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
                .write_pending_image(storage.select_table_definition(table, 8).0, 1, 1, Some(image), crate::storage::RowWriteStatistics::Untracked)
                .unwrap();
            storage.commit_row(table, 1, 8, 9).unwrap();
            storage.observe_lsn(9);
            drop(reader);
        });
        let versions = storage.row_versions.test_write().unwrap();
        let finished = AtomicBool::new(false);
        let progressed = std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                crate::mem::guard::forbid_alloc(|| finish_blocked_pass(&storage));
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
            finish_history_work(&storage);
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
                        assert_eq!(
                            storage.advance_history_cleanup(SLOTS_PER_BATCH),
                            HistoryReclamation::WaitingForOwner
                        );
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
                finish_history_work(&storage);
                assert!(!storage.snapshot_state().statement_reclamation_pending);
            });
        }
    }

    #[test]
    fn nonblocking_snapshot_batch_releases_prefix_owners_when_rows_are_busy() {
        let (storage, table, _) = fixture();
        crate::mem::guard::forbid_alloc(|| {
            let rows = storage.tables[table].rows.write();
            assert!(
                storage
                    .prune_snapshot_row_slots(
                        table,
                        0,
                        SLOTS_PER_BATCH,
                        ReclamationMode::SkipBusy,
                        PressureScan::Continue
                    )
                    .is_none()
            );
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
                    .write_pending_image(storage.select_table_definition(table, 8).0, 1, 1, Some(image), crate::storage::RowWriteStatistics::Untracked)
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
                crate::mem::guard::forbid_alloc(|| {
                    storage.advance_history_cleanup(SLOTS_PER_BATCH)
                });
            });
            crate::mem::guard::forbid_alloc(|| {
                if !storage.tables[table]
                    .rows
                    .get(&1)
                    .unwrap()
                    .history
                    .is_empty()
                {
                    assert!(storage.history_cleanup_pending());
                }
                finish_history_work(&storage);
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

    #[test]
    fn bounded_snapshot_work_counts_slots_through_lookup_shifts_and_reuse() {
        let mut config = test_config();
        config.max_tables = 1;
        config.table_rows = 128;
        config.large_object_pages = 1;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let table = storage
            .create_table(make_def("bounded_deleted", &[("id", ColType::Int4, true)]))
            .unwrap();
        storage.set_lsn(7);
        crate::mem::guard::forbid_alloc(|| {
            let mut deleted = RowState::committed_only_at(super::super::RowLoc::test(0, 0), 7);
            deleted.committed = None;
            for rowid in 0..128 {
                storage.tables[table].rows.insert(rowid, deleted).unwrap();
            }
            let released = storage.retain_statement_snapshot(99, |lsn| lsn).unwrap();
            drop(released);
            let one = NonZeroUsize::new(1).unwrap();
            assert_eq!(
                storage.advance_history_cleanup(one),
                HistoryReclamation::Advanced
            );
            assert_eq!(storage.tables[table].rows.len(), 127);
            assert!(storage.tables[table].rows.get(&0).is_none());
            assert_eq!(
                storage
                    .snapshot_reclamation
                    .workspace
                    .lock()
                    .unwrap()
                    .cursor
                    .unwrap()
                    .slot,
                1
            );
            storage.tables[table].rows.remove(&1).unwrap();
            storage.tables[table].rows.insert(1000, deleted).unwrap();
            storage.tables[table].rows.insert(1001, deleted).unwrap();
            let released = storage.retain_statement_snapshot(100, |lsn| lsn).unwrap();
            drop(released);
            assert_eq!(
                storage.advance_history_cleanup(one),
                HistoryReclamation::Advanced
            );
            assert_eq!(storage.tables[table].rows.len(), 127);
            assert!(storage.tables[table].rows.get(&1000).is_none());
            assert!(storage.tables[table].rows.get(&1001).is_some());
            finish_history_work(&storage);
            assert!(storage.tables[table].rows.is_empty());
            assert!(!storage.history_cleanup_pending());
        });
    }

    #[test]
    fn bounded_snapshot_release_during_a_partial_pass_remains_queued() {
        let (storage, table, _) = fixture();
        let reader = storage.retain_statement_snapshot(99, |lsn| lsn).unwrap();
        let state = storage.resident_row_state(table, 1).unwrap().unwrap();
        let selected = storage
            .visible_row_home_at(table, 1, state, 99, SNAPSHOT_ALL, 7)
            .unwrap()
            .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let image = storage.heap.append_row_image(&[Datum::Int4(20)]).unwrap();
            storage
                .write_pending_image(storage.select_table_definition(table, 8).0, 1, 1, Some(image), crate::storage::RowWriteStatistics::Untracked)
                .unwrap();
            storage.commit_row(table, 1, 8, 9).unwrap();
            storage.observe_lsn(9);
            let released = storage.retain_statement_snapshot(100, |lsn| lsn).unwrap();
            drop(released);
            let one = NonZeroUsize::new(1).unwrap();
            assert_eq!(
                storage.advance_history_cleanup(one),
                HistoryReclamation::Advanced
            );
            assert!(
                storage
                    .snapshot_reclamation
                    .workspace
                    .lock()
                    .unwrap()
                    .cursor
                    .is_some()
            );
            assert_eq!(storage.tables[table].rows.get(&1).unwrap().history.len(), 1);
            drop(reader);
            for _ in 0..=storage.tables.len() {
                let active = storage
                    .snapshot_reclamation
                    .workspace
                    .lock()
                    .unwrap()
                    .cursor
                    .is_some();
                if !active {
                    break;
                }
                assert_eq!(
                    storage.advance_history_cleanup(one),
                    HistoryReclamation::Advanced
                );
            }
            assert!(
                storage
                    .snapshot_reclamation
                    .workspace
                    .lock()
                    .unwrap()
                    .cursor
                    .is_none()
            );
            assert!(storage.snapshot_state().statement_reclamation_pending);
            assert_eq!(storage.tables[table].rows.get(&1).unwrap().history.len(), 1);
            finish_history_work(&storage);
            assert!(
                storage.tables[table]
                    .rows
                    .get(&1)
                    .unwrap()
                    .history
                    .is_empty()
            );
            storage
                .with_row_bytes(table, 1, selected, |bytes| {
                    let mut values = [Datum::Null];
                    crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                    assert_eq!(values[0], Datum::Int4(1));
                    Ok(())
                })
                .unwrap();
        });
    }

    #[test]
    fn bounded_snapshot_transaction_end_drains_work_without_reusing_live_lease() {
        let (storage, table, _) = fixture();
        let reader = storage.retain_statement_snapshot(99, |lsn| lsn).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            storage.register_snapshot(99, 7).unwrap();
            let image = storage.heap.append_row_image(&[Datum::Int4(20)]).unwrap();
            storage
                .write_pending_image(storage.select_table_definition(table, 8).0, 1, 1, Some(image), crate::storage::RowWriteStatistics::Untracked)
                .unwrap();
            storage.commit_row(table, 1, 8, 9).unwrap();
            storage.observe_lsn(9);
            let released = storage.retain_statement_snapshot(100, |lsn| lsn).unwrap();
            drop(released);
            assert_eq!(
                storage.advance_history_cleanup(NonZeroUsize::new(1).unwrap()),
                HistoryReclamation::Advanced
            );
            assert!(
                storage
                    .snapshot_reclamation
                    .workspace
                    .lock()
                    .unwrap()
                    .cursor
                    .is_some()
            );
            storage.end_transaction_snapshots(99, crate::storage::SnapshotCleanup::Finish);
            assert!(!storage.has_active_snapshots());
            assert!(
                storage.tables[table]
                    .rows
                    .get(&1)
                    .unwrap()
                    .history
                    .is_empty()
            );
            assert!(
                storage
                    .snapshot_reclamation
                    .workspace
                    .lock()
                    .unwrap()
                    .cursor
                    .is_none()
            );
            assert!(!storage.snapshot_reclamation.workspace.lock().unwrap().retry);
            assert_eq!(storage.snapshot_state().statements.len(), 1);
            drop(reader);
            assert!(storage.snapshot_state().statements.is_empty());
            finish_history_work(&storage);
            assert!(!storage.history_cleanup_pending());
        });
    }

    #[test]
    fn bounded_snapshot_pressure_signal_keeps_publication_after_an_earlier_slot() {
        let mut config = test_config();
        config.max_tables = 1;
        config.table_rows = 4;
        config.large_object_pages = 1;
        config.max_row_versions_per_row = 3;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let table = storage
            .create_table(make_def(
                "pressure_during_scan",
                &[("id", ColType::Int4, true)],
            ))
            .unwrap();
        let location = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        storage.tables[table]
            .rows
            .insert(1, RowState::committed_only_at(location, 7))
            .unwrap();
        storage.set_lsn(7);
        let reader = storage.retain_statement_snapshot(99, |lsn| lsn).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let released = storage.retain_statement_snapshot(100, |lsn| lsn).unwrap();
            drop(released);
            assert_eq!(
                storage.advance_history_cleanup(NonZeroUsize::new(1).unwrap()),
                HistoryReclamation::Advanced
            );
            assert!(!storage.history_pressure());
            let image = storage.heap.append_row_image(&[Datum::Int4(20)]).unwrap();
            storage
                .write_pending_image(storage.select_table_definition(table, 8).0, 1, 1, Some(image), crate::storage::RowWriteStatistics::Untracked)
                .unwrap();
            storage.commit_row(table, 1, 8, 9).unwrap();
            storage.observe_lsn(9);
            assert!(storage.history_pressure());
            finish_history_work(&storage);
            assert!(
                storage.history_pressure(),
                "a completed pass erased later publication"
            );
            assert_eq!(storage.tables[table].rows.get(&1).unwrap().history.len(), 1);
            drop(reader);
            finish_history_work(&storage);
            assert!(!storage.history_pressure());
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
    fn bounded_snapshot_obsolete_boundary_does_not_fill_one_version_row() {
        let mut config = test_config();
        config.max_tables = 1;
        config.table_rows = 4;
        config.large_object_pages = 1;
        config.max_row_versions_per_row = 1;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let table = storage
            .create_table(make_def("one_history_slot", &[("id", ColType::Int4, true)]))
            .unwrap();
        let location = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        storage.tables[table]
            .rows
            .insert(1, RowState::committed_only_at(location, 7))
            .unwrap();
        storage.set_lsn(7);
        let old = storage.retain_statement_snapshot(99, |lsn| lsn).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let image = storage.heap.append_row_image(&[Datum::Int4(20)]).unwrap();
            storage
                .write_pending_image(storage.select_table_definition(table, 8).0, 1, 1, Some(image), crate::storage::RowWriteStatistics::Untracked)
                .unwrap();
            storage.commit_row(table, 1, 8, 9).unwrap();
            storage.observe_lsn(9);
            drop(old);
            let current = storage.retain_statement_snapshot(100, |lsn| lsn).unwrap();
            assert_eq!(storage.tables[table].rows.get(&1).unwrap().history.len(), 1);
            let image = storage.heap.append_row_image(&[Datum::Int4(30)]).unwrap();
            storage
                .write_pending_image(storage.select_table_definition(table, 10).0, 1, 1, Some(image), crate::storage::RowWriteStatistics::Untracked)
                .unwrap();
            storage.commit_row(table, 1, 10, 11).unwrap();
            storage.observe_lsn(11);
            let state = storage.resident_row_state(table, 1).unwrap().unwrap();
            let selected = storage
                .visible_row_home_at(table, 1, state, 100, SNAPSHOT_ALL, current.snapshot())
                .unwrap()
                .unwrap();
            storage
                .with_row_bytes(table, 1, selected, |bytes| {
                    let mut values = [Datum::Null];
                    crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                    assert_eq!(values[0], Datum::Int4(20));
                    Ok(())
                })
                .unwrap();
            drop(current);
            finish_history_work(&storage);
            assert!(!storage.history_pressure());
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
}
