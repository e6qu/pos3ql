//! Owned, startup-bounded retention for executing statement snapshots.

use super::SnapshotCleanup;
use core::mem::size_of;
use core::num::NonZeroU64;
use {crate::mem::mutex::Mutex, std::sync::Arc};

use super::{DatabaseOid, LockState, SnapshotState, Storage};
use crate::config::Config;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::lock::WaitOwner;
use crate::{sql_err, stack_format};

pub(crate) struct SnapshotIdentifier<'name>(&'name str);

impl<'name> SnapshotIdentifier<'name> {
    pub(crate) fn parse(name: &'name str) -> Result<Self, SqlError> {
        if !name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'A'..=b'F').contains(&byte) || byte == b'-')
        {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "invalid snapshot identifier: \"{}\"",
                name
            ));
        }
        Ok(Self(name))
    }

    pub(crate) fn issued(connection: i32, lsn: u64) -> crate::util::StackStr<128> {
        stack_format!(128, "{connection:08X}-{lsn:08X}-1")
    }

    fn matches(&self, snapshot: &ExportedSnapshot) -> bool {
        self.0 == Self::issued(snapshot.connection, snapshot.lsn).as_str()
    }
}

pub(super) struct ExportedSnapshot {
    pub(super) connection: i32,
    pub(super) database: DatabaseOid,
    pub(super) lsn: u64,
}

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
        if self.transaction_mode == crate::config::TransactionMode::Locking {
            return None;
        }
        self.active
            .iter()
            .map(|(_, lsn)| *lsn)
            .chain(self.exported.iter().map(|snapshot| snapshot.lsn))
            .chain(self.statements.iter().filter_map(|statement| statement.lsn))
            .min()
    }

    pub(super) fn retains_for(&self, transaction: u32) -> bool {
        if self.transaction_mode == crate::config::TransactionMode::Locking {
            return false;
        }
        !self.active.is_empty()
            || !self.exported.is_empty()
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
    files: super::spill_generations::StatementFileLease,
}

impl StatementSnapshotLease {
    pub(crate) fn snapshot(&self) -> u64 {
        self.lsn
    }

    pub(crate) fn validate_storage(&self, storage: &Storage) -> Result<(), SqlError> {
        if !Arc::ptr_eq(&self.snapshots, &storage.snapshots) {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "statement snapshot belongs to another storage instance"
            ));
        }
        Ok(())
    }

    pub(crate) fn enter_files(
        &self,
        storage: &Storage,
    ) -> Result<super::StatementFileScope, SqlError> {
        self.validate_storage(storage)?;
        self.files.enter()
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
    pub(crate) fn export_replication_snapshot(
        &self,
        connection: i32,
        lsn: u64,
    ) -> Result<(), SqlError> {
        self.validate_transaction_isolation(crate::sql::ast::TransactionIsolation::RepeatableRead)?;
        if connection <= 0 {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "replication snapshot requires a live connection"
            ));
        }
        let mut snapshots = self.snapshot_state();
        if let Some(existing) = snapshots
            .exported
            .iter_mut()
            .find(|snapshot| snapshot.connection == connection)
        {
            // Replace under the retention guard. Releasing first could prune
            // the very history the replacement is intended to protect.
            *existing = ExportedSnapshot {
                connection,
                database: self.current_database_oid(),
                lsn,
            };
            snapshots.statement_reclamation_pending = true;
            return Ok(());
        }
        snapshots
            .exported
            .push(ExportedSnapshot {
                connection,
                database: self.current_database_oid(),
                lsn,
            })
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "too many exported replication snapshots"
                )
            })
    }

    pub(crate) fn invalidate_replication_snapshot(&self, connection: i32) {
        let mut snapshots = self.snapshot_state();
        let Some(index) = snapshots
            .exported
            .iter()
            .position(|snapshot| snapshot.connection == connection)
        else {
            return;
        };
        snapshots.exported.swap_remove(index);
        snapshots.statement_reclamation_pending = true;
        drop(snapshots);
        self.lock_state()
            .row
            .resource_released(crate::sql::lock::connection_wait_owner(connection));
    }

    /// Authentication and the importer's history pin are one transition.
    /// The callback only updates transaction-local state, after capacity has
    /// been checked; it must not call back into Storage or perform I/O.
    pub(crate) fn import_replication_snapshot(
        &self,
        identifier: SnapshotIdentifier<'_>,
        transaction: u32,
        accept: impl FnOnce(u64) -> Result<(), SqlError>,
    ) -> Result<(), SqlError> {
        self.validate_transaction_isolation(crate::sql::ast::TransactionIsolation::RepeatableRead)?;
        let mut snapshots = self.snapshot_state();
        let Some(exported) = snapshots
            .exported
            .iter()
            .find(|snapshot| identifier.matches(snapshot))
        else {
            return Err(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "snapshot \"{}\" does not exist",
                identifier.0
            ));
        };
        if exported.database != self.current_database_oid() {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "cannot import a snapshot from a different database"
            ));
        }
        let lsn = exported.lsn;
        let prior = snapshots
            .active
            .iter()
            .position(|(owner, _)| *owner == transaction);
        if prior.is_none() && snapshots.active.len() == snapshots.active.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "more than {} active historical snapshots",
                snapshots.active.capacity()
            ));
        }
        accept(lsn)?;
        if let Some(index) = prior {
            snapshots.active[index].1 = lsn;
        } else {
            snapshots
                .active
                .push((transaction, lsn))
                .expect("snapshot import capacity checked under its guard");
        }
        Ok(())
    }

    pub(crate) fn retain_statement_snapshot(
        &self,
        transaction: u32,
        select: impl FnOnce(u64) -> u64,
    ) -> Result<StatementSnapshotLease, SqlError> {
        let owner = self.transaction_wait_owner(transaction);
        let mut snapshots = self.snapshot_state();
        let visible = snapshots.admission.select(owner, &self.locks)?;
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
        let files = self.spill_roots.retain_statement()?;
        let lsn = select(visible);
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
            files,
        })
    }
    pub(crate) fn end_transaction_snapshots(&self, transaction: u32, cleanup: SnapshotCleanup) {
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
        snapshots.statement_reclamation_pending = true;
        drop(snapshots);
        if cleanup == SnapshotCleanup::Finish {
            self.prune_snapshot_versions();
        }
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

    fn import_snapshot(
        storage: &Storage,
        connection: i32,
        lsn: u64,
        transaction: u32,
        accept: impl FnOnce() -> Result<(), SqlError>,
    ) -> Result<(), SqlError> {
        let name = SnapshotIdentifier::issued(connection, lsn);
        storage.import_replication_snapshot(
            SnapshotIdentifier::parse(name.as_str())?,
            transaction,
            |captured| {
                assert_eq!(captured, lsn);
                accept()
            },
        )
    }

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

    fn assert_original_row(storage: &Storage, table: usize, transaction: u32) {
        let row = storage.resident_row_state(table, 1).unwrap().unwrap();
        let image = storage
            .visible_row_home_at(table, 1, row, transaction, crate::storage::SNAPSHOT_ALL, 7)
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
    }

    #[test]
    fn replication_snapshot_replacement_preserves_unselected_history() {
        let (storage, table) = fixture();
        crate::mem::guard::forbid_alloc(|| {
            storage.export_replication_snapshot(77, 7).unwrap();
            let location = storage.heap.append_row(&[Datum::Int4(20)]).unwrap();
            storage
                .write_pending(table, 1, 8, 1, Some(location))
                .unwrap();
            storage.commit_row(table, 1, 8, 9).unwrap();
            storage.export_replication_snapshot(77, 7).unwrap();
            storage.prune_snapshot_versions();
            assert_original_row(&storage, table, 99);
            import_snapshot(&storage, 77, 7, 99, || Ok(())).unwrap();
            storage.invalidate_replication_snapshot(77);
            assert_eq!(storage.oldest_snapshot(), Some(7));
            assert_original_row(&storage, table, 99);
            storage.end_transaction_snapshots(99, crate::storage::SnapshotCleanup::Finish);
            assert_eq!(storage.oldest_snapshot(), None);
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
    fn replication_snapshot_capacity_and_import_errors_preserve_prior_state() {
        let (storage, _) = fixture();
        let capacity = storage.snapshot_state().active.capacity();
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(storage.snapshot_state().exported.capacity(), 2);
            for connection in [0, -1] {
                assert_eq!(
                    storage
                        .export_replication_snapshot(connection, 7)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE
                );
            }
            storage.export_replication_snapshot(77, 7).unwrap();
            storage.export_replication_snapshot(88, 9).unwrap();
            assert_eq!(
                storage
                    .export_replication_snapshot(99, 11)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            // Export reservations do not consume transaction reservations.
            for index in 0..capacity {
                storage.register_snapshot(index as u32 + 1, 8).unwrap();
            }
            let mut accepted = false;
            let error = import_snapshot(&storage, 77, 7, u32::MAX, || {
                accepted = true;
                Ok(())
            })
            .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(!accepted);
            assert_eq!(storage.oldest_snapshot(), Some(7));
            import_snapshot(&storage, 77, 7, 1, || Ok(())).unwrap();
            assert_eq!(storage.snapshot_state().active.len(), capacity);
            storage.release_snapshot(2);
            let error = import_snapshot(&storage, 77, 7, u32::MAX, || {
                Err(sql_err!(sqlstate::ACTIVE_SQL_TRANSACTION, "already read"))
            })
            .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::ACTIVE_SQL_TRANSACTION);
            assert!(
                !storage
                    .snapshot_state()
                    .active
                    .iter()
                    .any(|(owner, _)| *owner == u32::MAX)
            );
            assert_eq!(
                import_snapshot(&storage, 77, 8, u32::MAX, || {
                    accepted = true;
                    Ok(())
                })
                .unwrap_err()
                .sqlstate,
                sqlstate::UNDEFINED_OBJECT
            );
            assert!(!accepted);
            storage.invalidate_replication_snapshot(77);
            assert_eq!(
                import_snapshot(&storage, 77, 7, u32::MAX, || {
                    accepted = true;
                    Ok(())
                })
                .unwrap_err()
                .sqlstate,
                sqlstate::UNDEFINED_OBJECT
            );
            assert!(!accepted);
            storage.export_replication_snapshot(99, 10).unwrap();
            assert_eq!(storage.snapshot_state().exported.len(), 2);
        });
    }

    #[test]
    fn replication_snapshot_database_rejection_preserves_importer_and_export() {
        let (storage, _) = fixture();
        crate::mem::guard::forbid_alloc(|| {
            storage.export_replication_snapshot(77, 7).unwrap();
            let prior = storage.current_database_oid();
            crate::sql::set_execution_database_oid(DatabaseOid::parse(16_384).unwrap());
            let mut accepted = false;
            let result = import_snapshot(&storage, 77, 7, 99, || {
                accepted = true;
                Ok(())
            });
            crate::sql::set_execution_database_oid(prior);
            assert_eq!(
                result.unwrap_err().sqlstate,
                sqlstate::FEATURE_NOT_SUPPORTED
            );
            assert!(!accepted);
            assert!(storage.snapshot_state().active.is_empty());
            import_snapshot(&storage, 77, 7, 99, || Ok(())).unwrap();
            storage.invalidate_replication_snapshot(77);
            assert_eq!(storage.oldest_snapshot(), Some(7));
            storage.release_snapshot(99);
        });
    }

    #[test]
    fn replication_snapshot_connection_ownership_cannot_alias_transactions() {
        let (storage, _) = fixture();
        crate::mem::guard::forbid_alloc(|| {
            storage.export_replication_snapshot(77, 7).unwrap();
            let formerly_synthetic = 0x8000_0000 | 77;
            storage.register_snapshot(formerly_synthetic, 9).unwrap();
            storage.end_transaction_snapshots(
                formerly_synthetic,
                crate::storage::SnapshotCleanup::Finish,
            );
            assert_eq!(storage.oldest_snapshot(), Some(7));
            let blocker = storage.schema_lock_blocker(99).unwrap();
            assert_eq!(blocker, super::super::SchemaLockBlocker::Connection(77));
            storage.wait_for_schema_lock(99, blocker).unwrap();
            let generation = storage.lock_generation();
            storage.invalidate_replication_snapshot(77);
            assert!(storage.lock_generation() > generation);
            assert_eq!(storage.schema_lock_blocker(99), None);
            assert_eq!(storage.oldest_snapshot(), None);
            storage.invalidate_replication_snapshot(77);
            assert_eq!(storage.snapshot_state().exported.len(), 0);
        });
    }

    #[test]
    fn replication_snapshot_expiration_does_not_wait_for_row_metadata() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};
        let (storage, table) = fixture();
        storage.export_replication_snapshot(77, 7).unwrap();
        let finished = AtomicBool::new(false);
        let start = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let reader = storage.resident_row_state(table, 1).unwrap().unwrap();
            scope.spawn(|| {
                start.wait();
                crate::mem::guard::forbid_alloc(|| {
                    storage.invalidate_replication_snapshot(77);
                    finished.store(true, Ordering::Release);
                });
            });
            start.wait();
            let deadline = Instant::now() + Duration::from_secs(2);
            crate::mem::guard::forbid_alloc(|| {
                while !finished.load(Ordering::Acquire) && Instant::now() < deadline {
                    std::thread::yield_now();
                }
            });
            let progressed = finished.load(Ordering::Acquire);
            // Release even on failure, so a regression reports an assertion
            // instead of leaving a blocked worker for the job deadline.
            drop(reader);
            assert!(
                progressed,
                "export expiration waited for row-version metadata"
            );
        });
        crate::mem::guard::forbid_alloc(|| {
            assert!(storage.snapshot_state().statement_reclamation_pending);
            storage.prune_snapshot_versions();
            assert!(!storage.snapshot_state().statement_reclamation_pending);
        });
    }

    #[test]
    fn replication_snapshot_import_and_expiration_are_atomic_with_writes() {
        use std::sync::atomic::{AtomicBool, Ordering};
        for _ in 0..12 {
            let (storage, table) = fixture();
            storage.export_replication_snapshot(77, 7).unwrap();
            let imported = AtomicBool::new(false);
            let start = std::sync::Barrier::new(3);
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    start.wait();
                    crate::mem::guard::forbid_alloc(|| {
                        let result = import_snapshot(&storage, 77, 7, 99, || {
                            imported.store(true, Ordering::Release);
                            Ok(())
                        });
                        if let Err(error) = result {
                            assert_eq!(error.sqlstate, sqlstate::UNDEFINED_OBJECT);
                            assert!(!imported.load(Ordering::Acquire));
                        }
                    });
                });
                scope.spawn(|| {
                    start.wait();
                    crate::mem::guard::forbid_alloc(|| storage.invalidate_replication_snapshot(77));
                });
                start.wait();
                crate::mem::guard::forbid_alloc(|| {
                    let location = storage.heap.append_row(&[Datum::Int4(20)]).unwrap();
                    storage
                        .write_pending(table, 1, 8, 1, Some(location))
                        .unwrap();
                    storage.commit_row(table, 1, 8, 9).unwrap();
                });
            });
            crate::mem::guard::forbid_alloc(|| {
                storage.prune_snapshot_versions();
                if imported.load(Ordering::Acquire) {
                    assert_eq!(storage.oldest_snapshot(), Some(7));
                    assert_original_row(&storage, table, 99);
                    storage.release_snapshot(99);
                } else {
                    assert_eq!(storage.oldest_snapshot(), None);
                }
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
            let state = storage.resident_row_state(table, 1).unwrap().unwrap();
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
            assert_eq!(
                storage.schema_lock_blocker(8),
                Some(super::super::SchemaLockBlocker::Transaction(99))
            );
            drop(reader);
            assert!(!storage.has_active_snapshots());
            assert_eq!(storage.schema_lock_blocker(8), None);
            storage.prune_snapshot_versions();
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
    fn statement_snapshot_leases_allow_publication_while_reader_bytes_are_pinned() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};

        let (storage, table) = fixture();
        let reader = crate::mem::guard::forbid_alloc(|| {
            storage.retain_statement_snapshot(99, |lsn| lsn).unwrap()
        });
        let image = crate::mem::guard::forbid_alloc(|| {
            let state = storage.resident_row_state(table, 1).unwrap().unwrap();
            storage
                .visible_row_home_at(
                    table,
                    1,
                    state,
                    99,
                    crate::storage::SNAPSHOT_ALL,
                    reader.snapshot(),
                )
                .unwrap()
                .unwrap()
        });
        let completed = AtomicBool::new(false);
        let mut progressed = false;
        std::thread::scope(|scope| {
            let storage = &storage;
            let completed = &completed;
            let writer = scope.spawn(move || {
                crate::mem::guard::forbid_alloc(|| {
                    let location = storage.heap.append_row(&[Datum::Int4(20)]).unwrap();
                    storage
                        .write_pending(table, 1, 8, 1, Some(location))
                        .unwrap();
                    storage.commit_row(table, 1, 8, 9).unwrap();
                    completed.store(true, Ordering::Release);
                })
            });
            crate::mem::guard::forbid_alloc(|| {
                let deadline = Instant::now() + Duration::from_secs(5);
                while !completed.load(Ordering::Acquire) && Instant::now() < deadline {
                    std::thread::yield_now();
                }
                progressed = completed.load(Ordering::Acquire);
                storage
                    .with_row_bytes(table, 1, image, |bytes| {
                        let mut values = [Datum::Null];
                        crate::storage::rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                        assert_eq!(values[0], Datum::Int4(1));
                        Ok(())
                    })
                    .unwrap();
                drop(reader);
            });
            writer.join().unwrap();
        });
        assert!(
            progressed,
            "writer must finish before the statement lease and selected bytes release"
        );
        crate::mem::guard::forbid_alloc(|| storage.prune_snapshot_versions());
        assert!(
            storage
                .resident_row_state(table, 1)
                .unwrap()
                .unwrap()
                .history
                .is_empty()
        );
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
                    .unwrap()
                    .history
                    .is_empty(),
                "a writer's own statement must not retain replaced images"
            );
            storage.end_transaction_snapshots(8, crate::storage::SnapshotCleanup::Finish);
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
