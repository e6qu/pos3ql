//! Owned, startup-bounded retention for executing statement snapshots.

use core::mem::size_of;
use core::num::NonZeroU64;
use std::sync::{Arc, Mutex};

use super::{LockState, SnapshotState, Storage};
use crate::config::Config;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::lock::WaitOwner;
use crate::sql_err;

pub(super) struct StatementSnapshot {
    identity: NonZeroU64,
    pub(super) transaction: u32,
    pub(super) lsn: Option<u64>,
}

const STATEMENT_SNAPSHOT_SLOTS_PER_WORKSPACE: usize = 1024;

pub(super) fn statement_snapshot_capacity(config: &Config) -> usize {
    config
        .query_workspace_slots
        .saturating_mul(STATEMENT_SNAPSHOT_SLOTS_PER_WORKSPACE)
}

pub(super) fn snapshot_owner_bytes() -> usize {
    size_of::<Mutex<SnapshotState>>()
        + size_of::<Mutex<LockState>>()
        + 4 * size_of::<std::sync::atomic::AtomicUsize>()
}

impl SnapshotState {
    pub(super) fn oldest(&self) -> Option<u64> {
        self.active
            .iter()
            .map(|(_, lsn)| *lsn)
            .chain(self.statements.iter().filter_map(|statement| statement.lsn))
            .min()
    }

    pub(super) fn retains_for(&self, transaction: u32) -> bool {
        !self.active.is_empty()
            || self
                .statements
                .iter()
                .any(|statement| statement.lsn.is_some() && statement.transaction != transaction)
    }
}

#[must_use = "the statement must retain its snapshot until execution ends"]
pub(crate) struct StatementSnapshotLease {
    snapshots: Arc<Mutex<SnapshotState>>,
    locks: Arc<Mutex<LockState>>,
    identity: NonZeroU64,
    owner: WaitOwner,
    lsn: u64,
}

impl StatementSnapshotLease {
    pub(crate) fn snapshot(&self) -> u64 {
        self.lsn
    }
}

impl Drop for StatementSnapshotLease {
    fn drop(&mut self) {
        let mut snapshots = self.snapshots.lock().expect("snapshot state lock poisoned");
        let position = snapshots
            .statements
            .iter()
            .position(|statement| statement.identity == self.identity)
            .expect("statement snapshot lease owns a live identity");
        snapshots.statements.swap_remove(position);
        snapshots.statement_reclamation_pending = true;
        drop(snapshots);
        self.locks
            .lock()
            .expect("lock state lock poisoned")
            .row
            .resource_released(self.owner);
    }
}

impl Storage {
    pub(crate) fn retain_statement_snapshot(
        &self,
        transaction: u32,
        select: impl FnOnce(u64) -> u64,
    ) -> Result<StatementSnapshotLease, SqlError> {
        self.prune_released_statement_snapshots();
        let owner = self.transaction_wait_owner(transaction);
        let mut snapshots = self.snapshot_state();
        let next = snapshots
            .next_statement
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "statement snapshot identity exhausted"
                )
            })?;
        if snapshots.statements.len() == snapshots.statements.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "statement snapshots are exhausted ({})",
                snapshots.statements.capacity()
            ));
        }
        let lsn = select(self.lsn());
        snapshots
            .statements
            .push(StatementSnapshot {
                identity: next,
                transaction,
                lsn: Some(lsn),
            })
            .map_err(|error| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "{}", error))?;
        snapshots.next_statement = next.get();
        Ok(StatementSnapshotLease {
            snapshots: Arc::clone(&self.snapshots),
            locks: Arc::clone(&self.locks),
            identity: next,
            owner,
            lsn,
        })
    }
    pub(crate) fn end_transaction_snapshots(&self, transaction: u32) {
        let mut snapshots = self.snapshot_state();
        if let Some(position) = snapshots
            .active
            .iter()
            .position(|(owner, _)| *owner == transaction)
        {
            snapshots.active.swap_remove(position);
        }
        for statement in snapshots
            .statements
            .iter_mut()
            .filter(|statement| statement.transaction == transaction)
        {
            statement.lsn = None;
        }
        self.prune_snapshot_versions(snapshots.oldest());
        drop(snapshots);
        let owner = self.transaction_wait_owner(transaction);
        self.lock_state().row.resource_released(owner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::types::{ColType, Datum};
    use crate::storage::RowState;
    use crate::storage::tests::{make_def, test_budget, test_config};

    fn fixture() -> (Storage, usize) {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let table = storage
            .create_table(make_def("statement_lease", &[("id", ColType::Int4, true)]))
            .unwrap();
        let location = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        storage.tables[table]
            .rows
            .insert(1, RowState::committed_only_at(location, 7))
            .unwrap();
        storage.set_lsn(7);
        (storage, table)
    }

    #[test]
    fn statement_snapshot_leases_preserve_reader_versions_and_release_history() {
        let (storage, table) = fixture();
        crate::mem::guard::forbid_alloc(|| {
            let reader = storage.retain_statement_snapshot(99, |lsn| lsn).unwrap();
            assert_eq!(reader.snapshot(), 7);
            let location = storage.heap.append_row(&[Datum::Int4(20)]).unwrap();
            storage
                .write_pending(table, 1, 8, 1, Some(location))
                .unwrap();
            storage.commit_row(table, 1, 8, 9).unwrap();
            let state = storage.resident_row_state(table, 1).unwrap();
            assert_eq!(state.history.len(), 1);
            let image = storage
                .visible_row_home_at(
                    table,
                    1,
                    state,
                    99,
                    crate::storage::SNAPSHOT_ALL,
                    reader.snapshot(),
                )
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
            assert_eq!(storage.schema_lock_blocker(8), Some(99));
            drop(reader);
            assert!(!storage.has_active_snapshots());
            assert_eq!(storage.schema_lock_blocker(8), None);
            storage.prune_released_statement_snapshots();
            assert!(
                storage
                    .resident_row_state(table, 1)
                    .unwrap()
                    .history
                    .is_empty()
            );
        });
    }

    #[test]
    fn statement_snapshot_leases_allow_publication_while_reader_bytes_are_pinned() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};

        let (storage, table) = fixture();
        let reader = crate::mem::guard::forbid_alloc(|| storage.retain_statement_snapshot(99, |lsn| lsn).unwrap());
        let image = crate::mem::guard::forbid_alloc(|| {
            let state = storage.resident_row_state(table, 1).unwrap();
            storage.visible_row_home_at(table, 1, state, 99, crate::storage::SNAPSHOT_ALL, reader.snapshot()).unwrap().unwrap()
        });
        let completed = AtomicBool::new(false);
        let mut progressed = false;
        std::thread::scope(|scope| {
            let storage = &storage;
            let completed = &completed;
            let writer = scope.spawn(move || crate::mem::guard::forbid_alloc(|| {
                let location = storage.heap.append_row(&[Datum::Int4(20)]).unwrap();
                storage.write_pending(table, 1, 8, 1, Some(location)).unwrap();
                storage.commit_row(table, 1, 8, 9).unwrap();
                completed.store(true, Ordering::Release);
            }));
            crate::mem::guard::forbid_alloc(|| {
                let deadline = Instant::now() + Duration::from_secs(5);
                while !completed.load(Ordering::Acquire) && Instant::now() < deadline { std::thread::yield_now(); }
                progressed = completed.load(Ordering::Acquire);
                storage.with_row_bytes(table, 1, image, |bytes| {
                    let mut values = [Datum::Null];
                    crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                    assert_eq!(values[0], Datum::Int4(1));
                    Ok(())
                }).unwrap();
                drop(reader);
            });
            writer.join().unwrap();
        });
        assert!(progressed, "writer must finish before the statement lease and selected bytes release");
        crate::mem::guard::forbid_alloc(|| storage.prune_released_statement_snapshots());
        assert!(storage.resident_row_state(table, 1).unwrap().history.is_empty());
    }

    #[test]
    fn statement_snapshot_leases_nest_without_overwriting_transaction_retention() {
        let (storage, _) = fixture();
        crate::mem::guard::forbid_alloc(|| {
            storage.register_snapshot(99, 3).unwrap();
            let outer = storage.retain_statement_snapshot(8, |_| 5).unwrap();
            let inner = storage.retain_statement_snapshot(8, |_| 7).unwrap();
            assert_eq!(storage.oldest_snapshot(), Some(3));
            storage.release_snapshot(99);
            assert_eq!(storage.oldest_snapshot(), Some(5));
            drop(inner);
            assert_eq!(storage.oldest_snapshot(), Some(5));
            drop(outer);
            assert_eq!(storage.oldest_snapshot(), None);
        });
    }

    #[test]
    fn statement_snapshot_leases_end_transaction_without_reusing_live_identities() {
        let (storage, table) = fixture();
        crate::mem::guard::forbid_alloc(|| {
            let outer = storage.retain_statement_snapshot(8, |_| 7).unwrap();
            let location = storage.heap.append_row(&[Datum::Int4(20)]).unwrap();
            storage
                .write_pending(table, 1, 8, 1, Some(location))
                .unwrap();
            storage.commit_row(table, 1, 8, 9).unwrap();
            assert!(
                storage
                    .resident_row_state(table, 1)
                    .unwrap()
                    .history
                    .is_empty(),
                "a writer's own statement must not retain replaced images"
            );
            storage.end_transaction_snapshots(8);
            assert_eq!(storage.oldest_snapshot(), None);
            let newer = storage.retain_statement_snapshot(9, |_| 9).unwrap();
            assert_ne!(outer.identity, newer.identity);
            drop(outer);
            assert_eq!(storage.oldest_snapshot(), Some(9));
            drop(newer);
        });
    }

    #[test]
    fn statement_snapshot_leases_exhaust_before_selecting_a_boundary() {
        let (storage, _) = fixture();
        let capacity = storage.snapshot_state().statements.capacity();
        let mut leases = Vec::with_capacity(capacity);
        crate::mem::guard::forbid_alloc(|| {
            for _ in 0..capacity {
                leases.push(storage.retain_statement_snapshot(8, |lsn| lsn).unwrap());
            }
            let before = storage.snapshot_state().next_statement;
            let error = storage
                .retain_statement_snapshot(8, |_| {
                    panic!("full retention must preflight before snapshot selection")
                })
                .err()
                .unwrap();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(storage.snapshot_state().next_statement, before);
            leases.clear();
            storage.snapshot_state().next_statement = u64::MAX;
            let error = storage
                .retain_statement_snapshot(8, |_| {
                    panic!("identity exhaustion must preflight before snapshot selection")
                })
                .err()
                .unwrap();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(storage.oldest_snapshot(), None);
        });
    }

    #[test]
    fn statement_snapshot_leases_outlive_storage_and_release_on_error() {
        let (storage, _) = fixture();
        let lease = crate::mem::guard::forbid_alloc(|| {
            storage.retain_statement_snapshot(8, |lsn| lsn).unwrap()
        });
        drop(storage);
        crate::mem::guard::forbid_alloc(|| drop(lease));
        let (storage, _) = fixture();
        let result: Result<(), SqlError> = crate::mem::guard::forbid_alloc(|| {
            let _lease = storage.retain_statement_snapshot(8, |lsn| lsn)?;
            Err(sql_err!(sqlstate::QUERY_CANCELED, "statement canceled"))
        });
        assert_eq!(result.unwrap_err().sqlstate, sqlstate::QUERY_CANCELED);
        assert_eq!(storage.oldest_snapshot(), None);
    }
}
