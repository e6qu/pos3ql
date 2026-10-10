//! Prepare row images without publication locks, then validate and publish.

use super::row_versions::RowVersionState;
use super::*;

struct PreparedPendingWrite {
    table: usize,
    incarnation: u64,
    rowid: u64,
    expected: Option<RowState>,
    pending_identity: Option<u64>,
    committed: Option<RowHome>,
    committed_lsn: u64,
    change: PendingChange,
    existed: bool,
    track_statistics: bool,
}

#[cfg(test)]
mod tests {
    use super::super::tests::{make_def, test_budget, test_config};
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    fn fixture(config: &Config) -> (Storage, usize) {
        let mut budget = test_budget(config);
        let mut storage = Storage::new(config, &mut budget).unwrap();
        let table = storage
            .create_table(make_def("publication", &[("id", ColType::Int4, false)]))
            .unwrap();
        (storage, table)
    }

    #[test]
    fn shared_row_publication_completes_with_a_retained_visible_reader() {
        let (mut storage, table) = fixture(&test_config());
        let old = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        storage.tables[table]
            .rows
            .insert(1, RowState::committed_only_at(old, 7))
            .unwrap();
        let row = storage.resident_row_state(table, 1).unwrap();
        let image = storage
            .visible_row_home_at(table, 1, row, 8, SNAPSHOT_ALL, 7)
            .unwrap()
            .unwrap();
        let completed = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let storage = &storage;
            let completed = &completed;
            let writer = scope.spawn(move || {
                crate::mem::guard::forbid_alloc(|| {
                    let next = storage.heap.append_row(&[Datum::Int4(2)]).unwrap();
                    let undo = storage.write_pending(table, 2, 7, 1, Some(next)).unwrap();
                    completed.store(true, Ordering::Release);
                    undo
                })
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while !completed.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::yield_now();
            }
            let progressed = completed.load(Ordering::Acquire);
            crate::mem::guard::forbid_alloc(|| {
                storage
                    .with_row_bytes(table, 1, image, |bytes| {
                        assert_eq!(bytes, &*storage.heap.get(old).unwrap());
                        Ok(())
                    })
                    .unwrap();
            });
            let undo = writer.join().unwrap();
            assert!(
                progressed,
                "publication must complete before the retained reader releases bytes"
            );
            crate::mem::guard::forbid_alloc(|| {
                assert_eq!(
                    storage
                        .resident_row_state(table, 2)
                        .unwrap()
                        .pending_last()
                        .unwrap()
                        .txid,
                    7
                );
                storage.restore_pending(table, 2, 7, undo);
                assert!(storage.resident_row_state(table, 2).is_none());
            });
        });
    }

    #[test]
    fn shared_row_publication_conflicting_preparations_cannot_both_publish() {
        let (storage, table) = fixture(&test_config());
        let location = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        let first = storage
            .prepare_pending_write(table, 1, 7, 1, Some(location), true)
            .unwrap();
        let second = storage
            .prepare_pending_write(table, 1, 8, 1, Some(location), true)
            .unwrap();
        let barrier = std::sync::Barrier::new(3);
        std::thread::scope(|scope| {
            let run = |prepared| {
                barrier.wait();
                crate::mem::guard::forbid_alloc(|| storage.publish_pending_write(prepared).unwrap())
            };
            let first = scope.spawn(move || run(first));
            let second = scope.spawn(move || run(second));
            barrier.wait();
            let results = [first.join().unwrap(), second.join().unwrap()];
            assert_eq!(
                results
                    .iter()
                    .filter(|result| matches!(result, PendingPublication::Written(_)))
                    .count(),
                1
            );
            let row = storage.resident_row_state(table, 1).unwrap();
            let owner = row.pending_last().unwrap().txid;
            assert_eq!(row.pending.len(), 1);
            assert!(results.iter().any(
                |result| matches!(result, PendingPublication::Wait(blocker, _) if *blocker == owner)
            ));
            drop(row);
            let loser = if owner == 7 { 8 } else { 7 };
            assert_eq!(
                storage
                    .write_pending(table, 1, loser, 1, Some(location))
                    .unwrap_err()
                    .sqlstate,
                sqlstate::INTERNAL_LOCK_WAIT
            );
        });
    }

    #[test]
    fn shared_row_publication_revalidates_reused_pending_slots_and_undo() {
        let (storage, table) = fixture(&test_config());
        let location = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let prior = storage
                .write_pending(table, 1, 7, 1, Some(location))
                .unwrap();
            let stale = storage
                .prepare_pending_write(table, 1, 7, 2, None, true)
                .unwrap();
            storage.restore_pending(table, 1, 7, prior);
            let current = storage
                .write_pending(table, 1, 7, 1, Some(location))
                .unwrap();
            assert_eq!(stale.expected, storage.tables[table].rows.get(&1));
            assert!(matches!(
                storage.publish_pending_write(stale).unwrap(),
                PendingPublication::Retry
            ));
            storage.restore_pending(table, 1, 7, prior);
            assert_eq!(
                storage.resident_row_state(table, 1).unwrap().pending.len(),
                1
            );
            let deletion = storage.write_pending(table, 1, 7, 2, None).unwrap();
            storage.restore_pending(table, 1, 7, current);
            assert_eq!(
                storage.resident_row_state(table, 1).unwrap().pending.len(),
                2
            );
            storage.restore_pending(table, 1, 7, deletion);
            storage.restore_pending(table, 1, 7, current);
            assert!(storage.resident_row_state(table, 1).is_none());
        });
    }

    #[test]
    fn shared_row_publication_parallel_writers_keep_images_and_statistics_coherent() {
        let mut config = test_config();
        config.max_connections = 4;
        let (storage, table) = fixture(&config);
        let barrier = std::sync::Barrier::new(5);
        std::thread::scope(|scope| {
            for worker in 0..4u32 {
                let storage = &storage;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    crate::mem::guard::forbid_alloc(|| {
                        for item in 1..=16 {
                            let rowid = u64::from(worker * 16 + item);
                            let location = storage
                                .heap
                                .append_row(&[Datum::Int4(rowid as i32)])
                                .unwrap();
                            storage
                                .write_pending(table, rowid, worker + 7, 1, Some(location))
                                .unwrap();
                        }
                    });
                });
            }
            barrier.wait();
        });
        crate::mem::guard::forbid_alloc(|| {
            for rowid in 1..=64 {
                let row = storage.resident_row_state(table, rowid).unwrap();
                let pending = row.pending_last().unwrap();
                assert_eq!(pending.txid, ((rowid - 1) / 16) as u32 + 7);
                let bytes = storage.heap.get(pending.loc.unwrap()).unwrap();
                let mut values = [Datum::Null];
                rowenc::decode(&bytes, &[ColType::Int4], &mut values).unwrap();
                assert_eq!(values, [Datum::Int4(rowid as i32)]);
            }
            let statistics = storage.cumulative_statistics();
            assert_eq!(statistics.relation_transactions.len(), 4);
            assert!(
                statistics
                    .relation_transactions
                    .iter()
                    .all(|entry| entry.n_tup_ins == 16 && entry.n_tup_upd == 0)
            );
        });
    }

    #[test]
    fn shared_row_publication_statistics_failure_is_hidden_from_readers() {
        let (storage, table) = fixture(&test_config());
        let location = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        let prepared = storage
            .prepare_pending_write(table, 1, 7, 1, Some(location), true)
            .unwrap();
        let mut statistics = storage.cumulative_statistics();
        while statistics.relation_transactions.len() < statistics.relation_transactions.capacity() {
            statistics
                .relation_transactions
                .push(RelationTransactionStatistics::new(8, table, 1))
                .unwrap();
        }
        std::thread::scope(|scope| {
            let storage = &storage;
            let writer = scope.spawn(move || {
                crate::mem::guard::forbid_alloc(|| {
                    storage
                        .publish_pending_write(prepared)
                        .err()
                        .unwrap()
                        .sqlstate
                })
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut publishing = false;
            while Instant::now() < deadline {
                if storage.tables[table].rows.test_write().is_err() {
                    publishing = true;
                    break;
                }
                std::thread::yield_now();
            }
            let reader = scope.spawn(move || {
                crate::mem::guard::forbid_alloc(|| storage.resident_row_state(table, 1).is_none())
            });
            drop(statistics);
            assert_eq!(writer.join().unwrap(), sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(
                reader.join().unwrap(),
                "a failed pending head must never become visible"
            );
            assert!(
                publishing,
                "writer must reach the statistics check under publication ownership"
            );
        });
        assert!(storage.row_versions.test_write().is_ok());
        assert!(storage.tables[table].rows.test_write().is_ok());
    }

    #[test]
    fn shared_row_publication_rollback_preserves_committed_deletion_markers() {
        let (mut storage, table) = fixture(&test_config());
        let location = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        let marker = RowState {
            committed: None,
            committed_lsn: 9,
            checkpoint_change_lsn: 9,
            history: CommittedHistory::empty(),
            pending: PendingVersions::empty(),
        };
        storage.tables[table].rows.insert(1, marker).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let undo = storage
                .write_pending(table, 1, 7, 1, Some(location))
                .unwrap();
            storage.restore_pending(table, 1, 7, undo);
            assert_eq!(storage.tables[table].rows.get(&1), Some(marker));
            let row = storage.resident_row_state(table, 1).unwrap();
            assert_eq!(row.visible_at(8, SNAPSHOT_ALL, 9), Some(None));
            assert_eq!(row.checkpoint_change_lsn, 9);
        });
    }

    #[test]
    fn shared_row_publication_capacity_failures_preserve_reusable_ownership() {
        let mut config = test_config();
        config.table_rows = 1;
        config.txn_rows = 1;
        config.max_row_versions_per_row = 1;
        let (storage, table) = fixture(&config);
        let location = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let undo = storage
                .write_pending(table, 1, 7, 1, Some(location))
                .unwrap();
            for rowid in [1, 2] {
                assert_eq!(
                    storage
                        .write_pending(table, rowid, 7, 2, None)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::PROGRAM_LIMIT_EXCEEDED
                );
            }
            assert_eq!(
                storage.resident_row_state(table, 1).unwrap().pending.len(),
                1
            );
            storage.restore_pending(table, 1, 7, undo);
            storage
                .write_pending(table, 2, 7, 2, Some(location))
                .unwrap();
            assert!(storage.row_versions.test_write().is_ok());
            assert!(storage.tables[table].rows.test_write().is_ok());
        });
    }

    #[test]
    fn shared_row_publication_pool_exhaustion_leaves_no_row_or_statistics() {
        let mut config = test_config();
        config.max_connections = 1;
        config.max_prepared_transactions = 0;
        config.txn_rows = 1;
        config.table_rows = 2;
        let (storage, table) = fixture(&config);
        let location = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let undo = storage
                .write_pending(table, 1, 7, 1, Some(location))
                .unwrap();
            assert_eq!(
                storage
                    .write_pending(table, 2, 7, 1, Some(location))
                    .unwrap_err()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert!(storage.resident_row_state(table, 2).is_none());
            assert_eq!(
                storage.cumulative_statistics().relation_transactions[0].n_tup_ins,
                1
            );
            storage.restore_pending(table, 1, 7, undo);
            storage
                .write_pending(table, 2, 7, 1, Some(location))
                .unwrap();
            assert!(storage.resident_row_state(table, 1).is_none());
        });
    }

    #[test]
    fn shared_row_publication_rejects_retired_table_preparations() {
        let (mut storage, table) = fixture(&test_config());
        let location = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        let stale = storage
            .prepare_pending_write(table, 1, 7, 1, Some(location), true)
            .unwrap();
        storage.drop_table(table);
        let replacement = storage
            .create_table(make_def("replacement", &[("id", ColType::Int4, false)]))
            .unwrap();
        assert_eq!(replacement, table);
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                storage.publish_pending_write(stale).err().unwrap().sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            assert!(storage.resident_row_state(table, 1).is_none());
            assert!(
                storage
                    .cumulative_statistics()
                    .relation_transactions
                    .is_empty()
            );
        });
    }

    #[test]
    fn shared_row_publication_preserves_active_history_on_capacity_failure() {
        let mut config = test_config();
        config.max_row_versions_per_row = 1;
        let (mut storage, table) = fixture(&config);
        let old = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        let current = storage.heap.append_row(&[Datum::Int4(2)]).unwrap();
        let mut state = RowState::committed_only_at(current, 9);
        push_committed_version(
            &mut storage.row_versions.exclusive().committed_row_versions,
            &mut state.history,
            1,
            CommittedVersion {
                home: Some(RowHome::Heap(old)),
                lsn: 7,
            },
        )
        .unwrap();
        storage.tables[table].rows.insert(1, state).unwrap();
        storage.register_snapshot(8, 7).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                storage
                    .write_pending(table, 1, 7, 1, None)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            let row = storage.resident_row_state(table, 1).unwrap();
            assert_eq!(
                row.visible_at(8, SNAPSHOT_ALL, 7),
                Some(Some(RowHome::Heap(old)))
            );
            assert!(row.pending.is_none());
        });
        storage.release_snapshot(8);
        crate::mem::guard::forbid_alloc(|| {
            storage.write_pending(table, 1, 7, 1, None).unwrap();
            assert!(
                storage
                    .resident_row_state(table, 1)
                    .unwrap()
                    .history
                    .is_empty()
            );
        });
    }
}

enum PendingPublication {
    Written(PendingWriteUndo),
    Retry,
    Wait(u32, &'static str),
}

impl Storage {
    /// Publishes one immutable pending image. Byte preparation and object I/O
    /// precede the short version-pool/map transition; retained heap readers
    /// do not exclude it. Conflicting transactions park through the wait graph.
    pub fn write_pending(
        &self,
        table_index: usize,
        rowid: u64,
        txid: u32,
        cid: u32,
        loc: Option<RowLoc>,
    ) -> Result<PendingWriteUndo, SqlError> {
        self.write_pending_inner(table_index, rowid, txid, cid, loc, true)
    }

    /// Rewrites and WAL reconstruction share publication without DML counts.
    pub(crate) fn write_pending_untracked(
        &self,
        table_index: usize,
        rowid: u64,
        txid: u32,
        cid: u32,
        loc: Option<RowLoc>,
    ) -> Result<PendingWriteUndo, SqlError> {
        self.write_pending_inner(table_index, rowid, txid, cid, loc, false)
    }

    pub(super) fn write_pending_inner(
        &self,
        table_index: usize,
        rowid: u64,
        txid: u32,
        cid: u32,
        loc: Option<RowLoc>,
        track_statistics: bool,
    ) -> Result<PendingWriteUndo, SqlError> {
        loop {
            let prepared =
                self.prepare_pending_write(table_index, rowid, txid, cid, loc, track_statistics)?;
            match self.publish_pending_write(prepared)? {
                PendingPublication::Written(undo) => return Ok(undo),
                PendingPublication::Retry => continue,
                PendingPublication::Wait(owner, message) => {
                    return Err(self.pending_write_wait(txid, owner, message));
                }
            }
        }
    }

    fn pending_write_wait(&self, txid: u32, owner: u32, message: &'static str) -> SqlError {
        match self.wait_for_transaction(txid, owner) {
            Err(error) => error,
            Ok(()) => sql_err!(sqlstate::INTERNAL_LOCK_WAIT, "{}", message),
        }
    }

    fn prepare_pending_write(
        &self,
        table: usize,
        rowid: u64,
        txid: u32,
        cid: u32,
        loc: Option<RowLoc>,
        track_statistics: bool,
    ) -> Result<PreparedPendingWrite, SqlError> {
        // Shared Storage ownership excludes relocation and table retirement.
        // Even reconstruction paths must validate the complete heap image.
        if let Some(location) = loc {
            self.heap.get(location)?;
        }
        let incarnation = {
            let definition = self.table_def(table, txid);
            if let Some(owner) = definition.locked_by_other(txid) {
                return Err(self.pending_write_wait(
                    txid,
                    owner,
                    "statement is waiting for a concurrent table definition change",
                ));
            }
            definition.identity().created_at
        };
        let (expected, pending_identity, existed) = match self.resident_row_state(table, rowid) {
            Some(state) => {
                if let Some(owner) = state.locked_by_other(txid) {
                    drop(state);
                    return Err(self.pending_write_wait(
                        txid,
                        owner,
                        "statement is waiting for a concurrent row update",
                    ));
                }
                (
                    Some(*state),
                    state.pending_head_identity(),
                    state
                        .visible_at(txid, SNAPSHOT_ALL, u64::MAX)
                        .flatten()
                        .is_some(),
                )
            }
            None => (None, None, false),
        };
        // No chain/map guard survives into a cold point read or comparison.
        let spilled = if expected.is_none() {
            self.spill_probe_at(table, rowid, u64::MAX)?
        } else {
            None
        };
        let committed = expected.map_or_else(
            || {
                spilled.and_then(|version| {
                    version.len.map(|len| RowHome::Spilled {
                        len,
                        sst: version.member,
                        commit_lsn: version.commit_lsn,
                    })
                })
            },
            |state| state.committed,
        );
        let committed_lsn = expected.map_or_else(
            || spilled.map_or(0, |version| version.commit_lsn),
            |state| state.committed_lsn,
        );
        let (changed_columns, changes_existence) =
            self.pending_change_footprint(table, rowid, committed, loc, txid, track_statistics)?;
        Ok(PreparedPendingWrite {
            table,
            incarnation,
            rowid,
            expected,
            pending_identity,
            committed,
            committed_lsn,
            change: PendingChange {
                txid,
                cid,
                loc,
                changed_columns,
                changes_existence,
            },
            existed: existed || (expected.is_none() && committed.is_some()),
            track_statistics,
        })
    }

    fn publish_pending_write(
        &self,
        prepared: PreparedPendingWrite,
    ) -> Result<PendingPublication, SqlError> {
        let PreparedPendingWrite {
            table,
            incarnation,
            rowid,
            expected,
            pending_identity,
            committed,
            committed_lsn,
            change,
            existed,
            track_statistics,
        } = prepared;
        let definition = self.tables[table].definition.read();
        if definition.identity.created_at != incarnation {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "pending row belongs to a retired table incarnation"
            ));
        }
        if let Some(owner) = definition
            .identity
            .existence
            .pending_txid()
            .or_else(|| definition.pending.map(|head| head.transaction))
            .filter(|&owner| owner != change.txid)
        {
            return Ok(PendingPublication::Wait(
                owner,
                "statement is waiting for a concurrent table definition change",
            ));
        }
        // Snapshot registration/retirement still requires exclusive Storage.
        // Do not retain its registry mutex while waiting for metadata readers.
        let oldest = self.oldest_snapshot();
        // Match reader lock ordering. Never perform byte or object reads while
        // owning these locks; statistics failure is undone before release.
        let mut versions = self.row_versions.write();
        let mut rows = self.tables[table].rows.write();
        let current = rows.get(&rowid).copied();
        if let Some(owner) = current.and_then(|state| {
            pending_last(&versions.pending_row_versions, state.pending)
                .filter(|head| head.txid != change.txid)
                .map(|head| head.txid)
        }) {
            return Ok(PendingPublication::Wait(
                owner,
                "statement is waiting for a concurrent row update",
            ));
        }
        let current_identity = current
            .and_then(|state| state.pending.tail)
            .map(|slot| versions.pending_row_versions[slot].identity);
        if current != expected || current_identity != pending_identity {
            return Ok(PendingPublication::Retry);
        }
        if current.is_none() && rows.len() == rows.capacity() {
            Self::evict_redundant_rows(&mut rows);
            if rows.len() == rows.capacity() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "table row limit reached ({} rows in memtable)",
                    rows.capacity()
                ));
            }
        }
        let mut state = current.unwrap_or(RowState {
            committed,
            committed_lsn,
            checkpoint_change_lsn: 0,
            history: CommittedHistory::empty(),
            pending: PendingVersions::empty(),
        });
        prune_committed_history(
            &mut versions.committed_row_versions,
            &mut state.history,
            oldest,
        );
        // Pruning releases slots, so publish its chain handles even if a later
        // capacity check fails. Readers can never traverse a released chain.
        if current.is_some() {
            *rows.get_mut(&rowid).expect("validated row") = state;
        }
        if oldest.is_some()
            && state.pending.is_none()
            && (state.committed.is_some() || state.committed_lsn != 0)
            && state.history.len() == self.max_row_versions_per_row
        {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "active snapshot history for row {} reached max_row_versions_per_row ({})",
                rowid,
                self.max_row_versions_per_row
            ));
        }
        let undo = push_pending_version(
            &mut versions.pending_row_versions,
            &mut state.pending,
            self.max_row_versions_per_row,
            change,
        )?;
        rows.insert(rowid, state)
            .expect("map capacity reserved under its owner");
        if track_statistics
            && let Err(error) =
                self.record_relation_write(change.txid, table, existed, change.loc.is_some())
        {
            Self::restore_pending_locked(&mut versions, &mut rows, rowid, change.txid, undo);
            return Err(error);
        }
        Ok(PendingPublication::Written(undo))
    }

    /// Undo checks and removes one exact head under the same publication locks.
    pub fn restore_pending(&self, table: usize, rowid: u64, txid: u32, undo: PendingWriteUndo) {
        let mut versions = self.row_versions.write();
        let mut rows = self.tables[table].rows.write();
        Self::restore_pending_locked(&mut versions, &mut rows, rowid, txid, undo);
    }

    fn restore_pending_locked(
        versions: &mut RowVersionState,
        rows: &mut FixedMap<u64, RowState>,
        rowid: u64,
        txid: u32,
        undo: PendingWriteUndo,
    ) {
        let Some(state) = rows.get_mut(&rowid) else {
            return;
        };
        let Some(slot) = state.pending.tail else {
            return;
        };
        let head = versions.pending_row_versions[slot];
        if head.identity != undo.identity || head.change.txid != txid {
            return;
        }
        pop_pending_version(&mut versions.pending_row_versions, &mut state.pending);
        // A committed deletion still shadows older immutable SST images.
        if (state.committed.is_none()
            && state.committed_lsn == 0
            && state.history.is_empty()
            && state.pending.is_none())
            || Self::redundant_spilled_row_state(state)
        {
            rows.remove(&rowid);
        }
    }

    pub(super) fn evict_redundant_rows(rows: &mut FixedMap<u64, RowState>) {
        loop {
            let mut batch = [0u64; 512];
            let mut n = 0usize;
            for (&rowid, state) in rows.iter() {
                if Self::redundant_spilled_row_state(state) {
                    batch[n] = rowid;
                    n += 1;
                    if n == batch.len() {
                        break;
                    }
                }
            }
            if n == 0 {
                return;
            }
            for rowid in &batch[..n] {
                rows.remove(rowid);
            }
        }
    }
}
