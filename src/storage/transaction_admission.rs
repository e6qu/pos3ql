//! Statement admission and the last completely installed transaction share an owner.

use std::sync::Arc;

use super::{LockState, SnapshotState, Storage};
use crate::mem::mutex::Mutex;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::lock::WaitOwner;
use crate::sql_err;

#[derive(Clone, Copy)]
enum Phase {
    Open,
    Installing { owner: WaitOwner },
    RecoveryRequired,
}

pub(super) struct TransactionAdmission {
    visible_lsn: u64,
    phase: Phase,
}

impl TransactionAdmission {
    pub(super) fn new(visible_lsn: u64) -> Self {
        Self {
            visible_lsn,
            phase: Phase::Open,
        }
    }

    pub(super) fn select(
        &self,
        waiter: WaitOwner,
        locks: &Mutex<LockState>,
    ) -> Result<u64, SqlError> {
        match self.phase {
            Phase::Open => Ok(self.visible_lsn),
            Phase::Installing { owner } if owner != waiter => {
                // Register while the admission owner is held. Completion cannot
                // wake the reader before its wait edge exists.
                locks
                    .lock()
                    .expect("lock state lock poisoned")
                    .row
                    .wait_for(waiter, owner)?;
                Err(sql_err!(
                    sqlstate::INTERNAL_LOCK_WAIT,
                    "statement is waiting for transaction publication"
                ))
            }
            Phase::Installing { .. } | Phase::RecoveryRequired => Err(recovery_required()),
        }
    }
}

fn recovery_required() -> SqlError {
    sql_err!(
        sqlstate::TRANSACTION_RESOLUTION_UNKNOWN,
        "commit completion requires recovery; connection must close"
    )
}

#[must_use = "an unfinished installation fences new readers until recovery"]
pub(crate) struct TransactionInstallation {
    snapshots: Arc<Mutex<SnapshotState>>,
    locks: Arc<Mutex<LockState>>,
    owner: WaitOwner,
    lsn: u64,
    completed: bool,
}

impl TransactionInstallation {
    pub(crate) fn complete(mut self) {
        let mut snapshots = self.snapshots.lock().expect("snapshot state lock poisoned");
        let admission = &mut snapshots.admission;
        assert!(
            matches!(admission.phase, Phase::Installing { owner } if owner == self.owner),
            "installation owns reader admission"
        );
        admission.visible_lsn = admission.visible_lsn.max(self.lsn);
        admission.phase = Phase::Open;
        self.completed = true;
        // Wake before releasing admission: a reused connection owner must not
        // have a later publication's wait edges removed by this completion.
        self.locks
            .lock()
            .expect("lock state lock poisoned")
            .row
            .publication_finished(self.owner);
        drop(snapshots);
    }
}

impl Drop for TransactionInstallation {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let mut snapshots = self.snapshots.lock().expect("snapshot state lock poisoned");
        let admission = &mut snapshots.admission;
        assert!(
            matches!(admission.phase, Phase::Installing { owner } if owner == self.owner),
            "installation owns reader admission"
        );
        admission.phase = Phase::RecoveryRequired;
        self.locks
            .lock()
            .expect("lock state lock poisoned")
            .row
            .publication_finished(self.owner);
        drop(snapshots);
    }
}

impl Storage {
    pub(crate) fn visible_lsn(&self) -> u64 {
        self.snapshot_state().admission.visible_lsn
    }

    /// Recovery has exclusive access; provisional WAL reservations do not use this path.
    pub(crate) fn restore_visible_lsn(&mut self, lsn: u64) {
        self.snapshot_state().admission.visible_lsn = lsn;
    }

    pub(crate) fn advance_visible_lsn(&self, lsn: u64) -> Result<(), SqlError> {
        let mut snapshots = self.snapshot_state();
        if !matches!(snapshots.admission.phase, Phase::Open) {
            return Err(recovery_required());
        }
        snapshots.admission.visible_lsn = snapshots.admission.visible_lsn.max(lsn);
        Ok(())
    }

    pub(crate) fn statement_snapshot_lsn(&self, transaction: u32) -> Result<u64, SqlError> {
        let owner = self.transaction_wait_owner(transaction);
        self.snapshot_state().admission.select(owner, &self.locks)
    }

    /// Call after durability, before the first committed row or catalog change.
    pub(crate) fn begin_transaction_installation(
        &self,
        transaction: u32,
        lsn: u64,
    ) -> Result<TransactionInstallation, SqlError> {
        if transaction == 0 {
            return Err(recovery_required());
        }
        let owner = self.transaction_wait_owner(transaction);
        let mut snapshots = self.snapshot_state();
        let admission = &mut snapshots.admission;
        if !matches!(admission.phase, Phase::Open) {
            return Err(recovery_required());
        }
        admission.phase = Phase::Installing { owner };
        Ok(TransactionInstallation {
            snapshots: Arc::clone(&self.snapshots),
            locks: Arc::clone(&self.locks),
            owner,
            lsn,
            completed: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TransactionMode;
    use crate::sql::types::{ColType, Datum};
    use crate::storage::tests::{make_def, test_budget, test_config};
    use crate::storage::{RowState, SNAPSHOT_ALL};

    fn fixture(mode: TransactionMode) -> (Storage, [usize; 2]) {
        let mut config = test_config();
        config.transaction_mode = mode;
        config.max_tables = 2;
        config.table_rows = 2;
        config.txn_rows = 2;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let reset = storage.shared_statistics_reset_times();
        assert_eq!(
            [
                reset.archiver,
                reset.background_writer,
                reset.checkpointer,
                reset.io,
                reset.recovery_prefetch,
                reset.wal
            ],
            [0; 6]
        );
        assert_eq!(reset.slru, [0; 8]);
        let tables = ["admission_orders", "admission_invoices"].map(|name| {
            let table = storage
                .create_table(make_def(name, &[("total", ColType::Int4, true)]))
                .unwrap();
            let location = storage.heap.append_row(&[Datum::Int4(10)]).unwrap();
            storage.tables[table]
                .rows
                .insert(1, RowState::committed_only_at(location, 7))
                .unwrap();
            table
        });
        storage.set_lsn(7);
        storage.restore_visible_lsn(7);
        (storage, tables)
    }

    fn install_row(storage: &Storage, table: usize) {
        let location = storage.heap.append_row(&[Datum::Int4(20)]).unwrap();
        storage
            .write_pending(table, 1, 8, 1, Some(location))
            .unwrap();
        storage.commit_row(table, 1, 8, 9).unwrap();
    }

    fn total(storage: &Storage, tables: [usize; 2], snapshot: u64) -> [i32; 2] {
        tables.map(|table| {
            let row = storage.resident_row_state(table, 1).unwrap().unwrap();
            let image = storage
                .visible_row_home_at(table, 1, row, 99, SNAPSHOT_ALL, snapshot)
                .unwrap()
                .unwrap();
            storage
                .with_row_bytes(table, 1, image, |bytes| {
                    let mut values = [Datum::Null];
                    crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                    let Datum::Int4(total) = values[0] else {
                        panic!("integer total");
                    };
                    Ok(total)
                })
                .unwrap()
        })
    }

    #[test]
    fn transaction_admission_keeps_old_readers_running_and_new_readers_out_of_partial_rows() {
        let (storage, tables) = fixture(TransactionMode::Mvcc);
        crate::mem::guard::forbid_alloc(|| {
            let old = storage.retain_statement_snapshot(99, |lsn| lsn).unwrap();
            let installing = storage.begin_transaction_installation(8, 9).unwrap();
            install_row(&storage, tables[0]);
            assert_eq!(total(&storage, tables, old.snapshot()), [10, 10]);
            assert_eq!(storage.visible_lsn(), 7);
            let before = storage.snapshot_state().next_statement;
            let error = storage
                .retain_statement_snapshot(100, |_| panic!("unadmitted selector ran"))
                .err()
                .unwrap();
            assert_eq!(error.sqlstate, sqlstate::INTERNAL_LOCK_WAIT);
            assert_eq!(storage.snapshot_state().next_statement, before);
            let generation = storage.lock_generation();
            install_row(&storage, tables[1]);
            assert_eq!(total(&storage, tables, old.snapshot()), [10, 10]);
            installing.complete();
            assert_ne!(storage.lock_generation(), generation);
            let new = storage.retain_statement_snapshot(100, |lsn| lsn).unwrap();
            assert_eq!(new.snapshot(), 9);
            assert_eq!(total(&storage, tables, new.snapshot()), [20, 20]);
            assert_eq!(total(&storage, tables, old.snapshot()), [10, 10]);
            drop(old);
            drop(new);
            storage.prune_snapshot_versions();
            for table in tables {
                assert!(
                    storage
                        .resident_row_state(table, 1)
                        .unwrap()
                        .unwrap()
                        .history
                        .is_empty()
                );
            }
        });
    }

    #[test]
    fn transaction_admission_locking_mode_waits_without_allocating_row_history() {
        let (storage, tables) = fixture(TransactionMode::Locking);
        crate::mem::guard::forbid_alloc(|| {
            let installing = storage.begin_transaction_installation(8, 9).unwrap();
            install_row(&storage, tables[0]);
            assert_eq!(
                storage.statement_snapshot_lsn(99).unwrap_err().sqlstate,
                sqlstate::INTERNAL_LOCK_WAIT
            );
            install_row(&storage, tables[1]);
            installing.complete();
            let reader = storage.retain_statement_snapshot(99, |lsn| lsn).unwrap();
            assert_eq!(total(&storage, tables, reader.snapshot()), [20, 20]);
            assert!(!storage.has_active_snapshots());
            assert_eq!(
                storage
                    .row_versions
                    .read()
                    .committed_row_versions
                    .capacity(),
                0
            );
            for table in tables {
                assert!(
                    storage
                        .resident_row_state(table, 1)
                        .unwrap()
                        .unwrap()
                        .history
                        .is_empty()
                );
            }
        });
    }

    #[test]
    fn transaction_admission_failure_fences_new_readers_and_wakes_waiters() {
        let (storage, tables) = fixture(TransactionMode::Mvcc);
        crate::mem::guard::forbid_alloc(|| {
            let old = storage.retain_statement_snapshot(99, |lsn| lsn).unwrap();
            let installing = storage.begin_transaction_installation(8, 9).unwrap();
            install_row(&storage, tables[0]);
            assert_eq!(
                storage.statement_snapshot_lsn(100).unwrap_err().sqlstate,
                sqlstate::INTERNAL_LOCK_WAIT
            );
            let generation = storage.lock_generation();
            drop(installing);
            assert_ne!(storage.lock_generation(), generation);
            assert_eq!(storage.visible_lsn(), 7);
            assert_eq!(total(&storage, tables, old.snapshot()), [10, 10]);
            assert_eq!(
                storage
                    .retain_statement_snapshot(100, |_| panic!("fenced selector ran"))
                    .err()
                    .unwrap()
                    .sqlstate,
                sqlstate::TRANSACTION_RESOLUTION_UNKNOWN
            );
            assert_eq!(
                storage.statement_snapshot_lsn(101).unwrap_err().sqlstate,
                sqlstate::TRANSACTION_RESOLUTION_UNKNOWN
            );
            assert_eq!(
                storage.advance_visible_lsn(9).unwrap_err().sqlstate,
                sqlstate::TRANSACTION_RESOLUTION_UNKNOWN
            );
            assert!(storage.begin_transaction_installation(9, 10).is_err());
        });
    }

    #[test]
    fn transaction_admission_reserved_positions_do_not_advance_the_reader_frontier() {
        let (storage, tables) = fixture(TransactionMode::Mvcc);
        crate::mem::guard::forbid_alloc(|| {
            storage.observe_lsn(500);
            assert_eq!(storage.bump_lsn().resolve().unwrap(), 501);
            let reader = storage.retain_statement_snapshot(99, |lsn| lsn).unwrap();
            assert_eq!(reader.snapshot(), 7);
            assert_eq!(total(&storage, tables, reader.snapshot()), [10, 10]);
            storage.advance_visible_lsn(9).unwrap();
            storage.advance_visible_lsn(8).unwrap();
            assert_eq!(storage.statement_snapshot_lsn(100).unwrap(), 9);
        });
    }

    #[test]
    fn transaction_admission_replay_positions_do_not_publish_an_unfinished_transaction() {
        let (mut storage, tables) = fixture(TransactionMode::Mvcc);
        crate::mem::guard::forbid_alloc(|| {
            let installing = storage.begin_transaction_installation(8, 9).unwrap();
            install_row(&storage, tables[0]);
            storage.set_lsn(500);
            assert_eq!(storage.visible_lsn(), 7);
            assert_eq!(
                storage.statement_snapshot_lsn(99).unwrap_err().sqlstate,
                sqlstate::INTERNAL_LOCK_WAIT
            );
            install_row(&storage, tables[1]);
            installing.complete();
            let reader = storage.retain_statement_snapshot(99, |lsn| lsn).unwrap();
            assert_eq!(reader.snapshot(), 9);
            assert_eq!(total(&storage, tables, reader.snapshot()), [20, 20]);
        });
    }

    #[test]
    fn transaction_admission_completion_wakes_after_intermediate_owner_cleanup() {
        for success in [true, false] {
            let (storage, _) = fixture(TransactionMode::Mvcc);
            crate::mem::guard::forbid_alloc(|| {
                let installing = storage.begin_transaction_installation(8, 9).unwrap();
                assert_eq!(
                    storage.statement_snapshot_lsn(99).unwrap_err().sqlstate,
                    sqlstate::INTERNAL_LOCK_WAIT
                );
                storage.end_transaction_snapshots(8, crate::storage::SnapshotCleanup::Schedule);
                let generation = storage.lock_generation();
                if success {
                    installing.complete();
                } else {
                    drop(installing);
                }
                assert_ne!(
                    storage.lock_generation(),
                    generation,
                    "a parked reader missed final publication after its wait edge was removed"
                );
                if success {
                    assert_eq!(storage.statement_snapshot_lsn(99).unwrap(), 9);
                } else {
                    assert_eq!(
                        storage.statement_snapshot_lsn(99).unwrap_err().sqlstate,
                        sqlstate::TRANSACTION_RESOLUTION_UNKNOWN
                    );
                }
            });
        }
    }

    #[test]
    fn transaction_admission_rejects_empty_and_overlapping_owners_without_changes() {
        let (storage, _) = fixture(TransactionMode::Mvcc);
        crate::mem::guard::forbid_alloc(|| {
            assert!(storage.begin_transaction_installation(0, 9).is_err());
            let installing = storage.begin_transaction_installation(8, 9).unwrap();
            assert!(storage.begin_transaction_installation(9, 10).is_err());
            assert_eq!(
                storage.statement_snapshot_lsn(8).unwrap_err().sqlstate,
                sqlstate::TRANSACTION_RESOLUTION_UNKNOWN
            );
            installing.complete();
            assert_eq!(storage.statement_snapshot_lsn(99).unwrap(), 9);
            storage
                .begin_transaction_installation(9, 10)
                .unwrap()
                .complete();
            assert_eq!(storage.statement_snapshot_lsn(99).unwrap(), 10);
        });
    }

    #[test]
    fn transaction_admission_does_not_wait_for_existing_queries_to_finish() {
        let (storage, tables) = fixture(TransactionMode::Mvcc);
        let captured = std::sync::Barrier::new(5);
        let published = std::sync::Barrier::new(5);
        std::thread::scope(|scope| {
            let workers: [_; 4] = core::array::from_fn(|index| {
                let storage = &storage;
                let captured = &captured;
                let published = &published;
                scope.spawn(move || {
                    crate::mem::guard::forbid_alloc(|| {
                        let reader = storage
                            .retain_statement_snapshot(100 + u32::try_from(index).unwrap(), |lsn| {
                                lsn
                            })
                            .unwrap();
                        captured.wait();
                        published.wait();
                        assert_eq!(total(storage, tables, reader.snapshot()), [10, 10]);
                    })
                })
            });
            captured.wait();
            crate::mem::guard::forbid_alloc(|| {
                let installing = storage.begin_transaction_installation(8, 9).unwrap();
                for table in tables {
                    install_row(&storage, table);
                }
                installing.complete();
                let new = storage.retain_statement_snapshot(99, |lsn| lsn).unwrap();
                assert_eq!(total(&storage, tables, new.snapshot()), [20, 20]);
            });
            published.wait();
            for worker in workers {
                worker.join().unwrap();
            }
        });
    }
}
