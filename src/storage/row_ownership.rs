//! Acquire related owners without retaining a prefix while another lock waits.

use core::mem::{align_of, size_of};
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};

use super::row_versions::{RowVersionPools, RowVersionState};
use super::{RowState, Table};
use crate::config::Config;
use crate::mem::arena::Arena;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_map::FixedMap;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

trait WaitMutex {
    fn wait(&self, poison: &'static str);
}

impl<T> WaitMutex for Mutex<T> {
    fn wait(&self, poison: &'static str) {
        drop(self.lock().unwrap_or_else(|_| panic!("{poison}")));
    }
}

trait WaitRwLock {
    fn wait_read(&self, poison: &'static str);
    fn wait_write(&self, poison: &'static str);
}

impl<T> WaitRwLock for RwLock<T> {
    fn wait_read(&self, poison: &'static str) {
        drop(self.read().unwrap_or_else(|_| panic!("{poison}")));
    }

    fn wait_write(&self, poison: &'static str) {
        drop(self.write().unwrap_or_else(|_| panic!("{poison}")));
    }
}

enum WaitTarget<'lock> {
    Mutex(&'lock dyn WaitMutex),
    Read(&'lock dyn WaitRwLock),
    Write(&'lock dyn WaitRwLock),
}

/// Contains only the contended lock, never an earlier acquired guard.
pub(super) struct LockWait<'lock> {
    target: WaitTarget<'lock>,
    poison: &'static str,
}

impl LockWait<'_> {
    fn wait(self) {
        match self.target {
            WaitTarget::Mutex(lock) => lock.wait(self.poison),
            WaitTarget::Read(lock) => lock.wait_read(self.poison),
            WaitTarget::Write(lock) => lock.wait_write(self.poison),
        }
    }
}

pub(super) fn try_mutex<'lock, T>(
    lock: &'lock Mutex<T>,
    poison: &'static str,
) -> Result<MutexGuard<'lock, T>, LockWait<'lock>> {
    match lock.try_lock() {
        Ok(guard) => Ok(guard),
        Err(TryLockError::WouldBlock) => Err(LockWait {
            target: WaitTarget::Mutex(lock),
            poison,
        }),
        Err(TryLockError::Poisoned(_)) => panic!("{poison}"),
    }
}

pub(super) fn try_read<'lock, T>(
    lock: &'lock RwLock<T>,
    poison: &'static str,
) -> Result<RwLockReadGuard<'lock, T>, LockWait<'lock>> {
    match lock.try_read() {
        Ok(guard) => Ok(guard),
        Err(TryLockError::WouldBlock) => Err(LockWait {
            target: WaitTarget::Read(lock),
            poison,
        }),
        Err(TryLockError::Poisoned(_)) => panic!("{poison}"),
    }
}

pub(super) fn try_write<'lock, T>(
    lock: &'lock RwLock<T>,
    poison: &'static str,
) -> Result<RwLockWriteGuard<'lock, T>, LockWait<'lock>> {
    match lock.try_write() {
        Ok(guard) => Ok(guard),
        Err(TryLockError::WouldBlock) => Err(LockWait {
            target: WaitTarget::Write(lock),
            poison,
        }),
        Err(TryLockError::Poisoned(_)) => panic!("{poison}"),
    }
}

/// The closure only acquires owners. Mutations run once after it succeeds.
/// Returning a wait drops the closure's earlier guards before waiting alone.
pub(super) fn acquire<'lock, Owners>(
    mut attempt: impl FnMut() -> Result<Owners, LockWait<'lock>>,
) -> Owners {
    loop {
        match attempt() {
            Ok(owners) => return owners,
            Err(wait) => wait.wait(),
        }
    }
}

type RowMapWrite<'storage> = RwLockWriteGuard<'storage, FixedMap<u64, RowState>>;

struct RowMapAttempt<'slots, 'storage> {
    slots: &'slots mut [Option<RowMapWrite<'storage>>],
}

impl<'storage> RowMapAttempt<'_, 'storage> {
    fn acquire_maps(&mut self, tables: &'storage [Table]) -> Result<(), LockWait<'storage>> {
        for (slot, table) in tables.iter().enumerate() {
            self.slots[slot] = Some(table.rows.try_write_for_batch()?);
        }
        Ok(())
    }
}

impl Drop for RowMapAttempt<'_, '_> {
    fn drop(&mut self) {
        for owner in self.slots.iter_mut() {
            drop(owner.take());
        }
    }
}

/// Constructed only after every row map is owned; no map can change during
/// preflight and application, and neither pass acquires another row-map lock.
pub(super) struct LockedRowMaps<'slots, 'storage> {
    owners: RowMapAttempt<'slots, 'storage>,
}

impl LockedRowMaps<'_, '_> {
    pub(super) fn iter(&self) -> impl Iterator<Item = &FixedMap<u64, RowState>> {
        self.owners
            .slots
            .iter()
            .map(|owner| &**owner.as_ref().expect("all row maps acquired"))
    }

    pub(super) fn iter_mut(&mut self) -> impl Iterator<Item = &mut FixedMap<u64, RowState>> {
        self.owners
            .slots
            .iter_mut()
            .map(|owner| &mut **owner.as_mut().expect("all row maps acquired"))
    }
}

struct HeapRows<'slots, 'storage> {
    versions: RwLockWriteGuard<'storage, RowVersionState>,
    rows: LockedRowMaps<'slots, 'storage>,
}

fn try_heap_rows<'slots, 'storage>(
    versions: &'storage RowVersionPools,
    tables: &'storage [Table],
    slots: &'slots mut [Option<RowMapWrite<'storage>>],
) -> Result<HeapRows<'slots, 'storage>, LockWait<'storage>> {
    let mut owners = RowMapAttempt { slots };
    let versions = versions.try_write_for_batch()?;
    owners.acquire_maps(tables)?;
    Ok(HeapRows {
        versions,
        rows: LockedRowMaps { owners },
    })
}

pub(super) struct HeapRowOwnership {
    scratch: Mutex<Arena>,
}

impl HeapRowOwnership {
    fn scratch_bytes(config: &Config) -> usize {
        config
            .max_tables
            .saturating_add(1)
            .saturating_mul(size_of::<Option<RowMapWrite<'static>>>())
            .saturating_add(align_of::<Option<RowMapWrite<'static>>>() - 1)
    }

    pub(super) fn budget_bytes(config: &Config) -> usize {
        size_of::<Self>().saturating_add(Self::scratch_bytes(config))
    }

    pub(super) fn new(config: &Config, budget: &mut Budget) -> Result<Self, BudgetError> {
        budget.draw(size_of::<Self>(), "heap row-map ownership controls")?;
        Ok(Self {
            scratch: Mutex::new(Arena::new(
                budget,
                "heap row-map ownership",
                Self::scratch_bytes(config),
            )?),
        })
    }

    pub(super) fn with_all<R>(
        &self,
        versions: &RowVersionPools,
        tables: &[Table],
        operation: impl FnOnce(&mut RowVersionState, &mut LockedRowMaps<'_, '_>) -> Result<R, SqlError>,
    ) -> Result<R, SqlError> {
        // Only maintenance uses this scratch. Wait for it before taking any
        // version or table owner, and retain no guards between server turns.
        let mut scratch = self
            .scratch
            .lock()
            .expect("heap row ownership scratch poisoned");
        scratch.reset();
        let mut slots = scratch
            .alloc_owned_slice_with(tables.len(), |_| None)
            .map_err(|error| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "heap row-map ownership workspace exhausted: {}",
                    error
                )
            })?;
        loop {
            match try_heap_rows(versions, tables, &mut slots) {
                Ok(mut owners) => return operation(&mut owners.versions, &mut owners.rows),
                Err(wait) => wait.wait(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::types::{ColType, Datum};
    use crate::storage::tests::{make_def, test_budget, test_config};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    #[test]
    fn row_lock_owners_release_prefix_before_wait_and_mutate_once() {
        for mode in 0..3 {
            let prefix = Mutex::new(0usize);
            let local_mutex = Mutex::new(());
            let local_rw = RwLock::new(());
            let held_mutex = (mode == 0).then(|| local_mutex.lock().unwrap());
            let held_write = (mode == 1).then(|| local_rw.write().unwrap());
            let held_read = (mode == 2).then(|| local_rw.read().unwrap());
            let waiting = AtomicBool::new(false);
            let attempts = AtomicUsize::new(0);
            let mutations = AtomicUsize::new(0);
            let start = std::sync::Barrier::new(2);
            let mut progressed = false;
            let mut reached_wait = false;
            std::thread::scope(|scope| {
                let prefix = &prefix;
                let local_mutex = &local_mutex;
                let local_rw = &local_rw;
                let waiting = &waiting;
                let attempts = &attempts;
                let mutations = &mutations;
                let start = &start;
                let worker = scope.spawn(move || {
                    start.wait();
                    crate::mem::guard::forbid_alloc(|| {
                        let (mut prefix, _mutex, _read, _write) = acquire(|| {
                            let prefix = try_mutex(prefix, "prefix poisoned")?;
                            attempts.fetch_add(1, Ordering::Relaxed);
                            let mut mutex = None;
                            let mut read = None;
                            let mut write = None;
                            match mode {
                                0 => {
                                    mutex =
                                        Some(try_mutex(local_mutex, "local poisoned").inspect_err(
                                            |_| waiting.store(true, Ordering::Release),
                                        )?)
                                }
                                1 => {
                                    read =
                                        Some(try_read(local_rw, "local poisoned").inspect_err(
                                            |_| waiting.store(true, Ordering::Release),
                                        )?)
                                }
                                _ => {
                                    write =
                                        Some(try_write(local_rw, "local poisoned").inspect_err(
                                            |_| waiting.store(true, Ordering::Release),
                                        )?)
                                }
                            }
                            Ok((prefix, mutex, read, write))
                        });
                        *prefix += 1;
                        mutations.fetch_add(1, Ordering::Relaxed);
                    });
                });
                start.wait();
                crate::mem::guard::forbid_alloc(|| {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while !waiting.load(Ordering::Acquire) && Instant::now() < deadline {
                        std::thread::yield_now();
                    }
                    reached_wait = waiting.load(Ordering::Acquire);
                    while Instant::now() < deadline {
                        if let Ok(owner) = prefix.try_lock() {
                            assert_eq!(*owner, 0);
                            progressed = true;
                            break;
                        }
                        std::thread::yield_now();
                    }
                });
                // Release the contended owner even when a regression prevents progress.
                drop(held_mutex);
                drop(held_write);
                drop(held_read);
                worker.join().unwrap();
            });
            assert!(reached_wait);
            assert!(progressed, "a local wait retained an earlier global owner");
            assert!(attempts.load(Ordering::Relaxed) >= 2);
            assert_eq!(mutations.load(Ordering::Relaxed), 1);
            assert_eq!(*prefix.lock().unwrap(), 1);
        }
    }

    #[test]
    fn row_lock_owners_heap_budget_matches_startup_and_rejects_short_reservations() {
        let mut config = test_config();
        for maximum in [0, 1, 64] {
            config.max_tables = maximum;
            let bytes = HeapRowOwnership::budget_bytes(&config);
            let mut budget = Budget::new(bytes);
            let _workspace = HeapRowOwnership::new(&config, &mut budget).unwrap();
            assert_eq!(budget.used(), bytes);

            let mut short = Budget::new(bytes - 1);
            let error = HeapRowOwnership::new(&config, &mut short).err().unwrap();
            assert_eq!(error.what, "heap row-map ownership");
            assert_eq!(error.requested, HeapRowOwnership::scratch_bytes(&config));

            let mut short = Budget::new(size_of::<HeapRowOwnership>() - 1);
            let error = HeapRowOwnership::new(&config, &mut short).err().unwrap();
            assert_eq!(error.what, "heap row-map ownership controls");
        }
        for maximum in [usize::MAX, usize::MAX / 2] {
            config.max_tables = maximum;
            assert_eq!(HeapRowOwnership::budget_bytes(&config), usize::MAX);
            let mut budget = Budget::new(1024);
            crate::mem::guard::forbid_alloc(|| {
                let error = HeapRowOwnership::new(&config, &mut budget).err().unwrap();
                assert_eq!(error.what, "heap row-map ownership");
                assert_eq!(error.requested, usize::MAX);
            });
        }
    }

    #[test]
    fn row_lock_owners_heap_partial_acquisition_error_and_exhaustion_release_all() {
        let mut config = test_config();
        config.max_tables = 3;
        config.table_rows = 1;
        config.txn_rows = 1;
        config.large_object_pages = 1;
        config.max_connections = 1;
        config.max_prepared_transactions = 0;
        config.max_row_versions_per_row = 1;
        let mut budget = test_budget(&config);
        let mut storage = super::super::Storage::new(&config, &mut budget).unwrap();
        let first = storage
            .create_table(make_def("owner_first", &[("id", ColType::Int4, false)]))
            .unwrap();
        let blocked = storage
            .create_table(make_def("owner_blocked", &[("id", ColType::Int4, false)]))
            .unwrap();
        let other = storage
            .create_table(make_def("owner_other", &[("id", ColType::Int4, false)]))
            .unwrap();
        assert!(first < blocked);
        let mut tiny_config = test_config();
        tiny_config.max_tables = 0;
        let mut tiny_budget = Budget::new(HeapRowOwnership::budget_bytes(&tiny_config));
        let tiny = HeapRowOwnership::new(&tiny_config, &mut tiny_budget).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let held = storage.tables[blocked].rows.write();
            {
                let mut scratch = storage.heap_row_ownership.scratch.lock().unwrap();
                scratch.reset();
                let mut slots = scratch
                    .alloc_owned_slice_with(storage.tables.len(), |_| None)
                    .unwrap();
                let wait = try_heap_rows(&storage.row_versions, &storage.tables, &mut slots)
                    .err()
                    .unwrap();
                assert!(slots.iter().all(Option::is_none));
                assert!(storage.row_versions.test_write().is_ok());
                assert!(storage.tables[first].rows.test_write().is_ok());

                let image = storage.heap.append_row_image(&[Datum::Int4(20)]).unwrap();
                let undo = storage
                    .write_pending_image_untracked(other, 1, 9, 1, Some(image))
                    .unwrap();
                storage.restore_pending(other, 1, 9, undo);
                assert!(storage.tables[other].rows.is_empty());
                drop(wait);
            }
            drop(held);

            let error: Result<(), SqlError> = storage.heap_row_ownership.with_all(
                &storage.row_versions,
                &storage.tables,
                |_, rows| {
                    assert_eq!(rows.iter().count(), storage.tables.len());
                    Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "forced ownership operation failure"
                    ))
                },
            );
            assert_eq!(error.unwrap_err().sqlstate, sqlstate::INTERNAL_ERROR);
            assert!(storage.row_versions.test_write().is_ok());
            assert!(
                storage
                    .tables
                    .iter()
                    .all(|table| table.rows.test_write().is_ok())
            );

            let mut calls = 0;
            storage
                .heap_row_ownership
                .with_all(&storage.row_versions, &storage.tables, |_, rows| {
                    calls += 1;
                    assert_eq!(rows.iter().count(), storage.tables.len());
                    Ok(())
                })
                .unwrap();
            assert_eq!(calls, 1);

            let error: Result<(), SqlError> =
                tiny.with_all(&storage.row_versions, &storage.tables, |_, _| {
                    panic!("exhausted scratch ran the operation")
                });
            let error = error.unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(
                error
                    .message
                    .as_str()
                    .starts_with("heap row-map ownership workspace exhausted:")
            );
            assert!(storage.row_versions.test_write().is_ok());
            assert!(
                storage
                    .tables
                    .iter()
                    .all(|table| table.rows.test_write().is_ok())
            );
        });
    }
}
