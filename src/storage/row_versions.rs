//! One ownership boundary for row-version arrays and their free lists.

use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use core::mem::size_of;
use core::ops::{Index, IndexMut};
use std::sync::{RwLock, RwLockReadGuard};

use super::{
    CommittedHistory, CommittedVersion, CommittedVersionSlot, PendingChange, PendingVersionSlot,
    PendingVersions, RowHome, RowLoc, committed_row_version_capacity, pending_row_version_capacity,
};
use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;

/// The allocator's reusable slots always belong to its own backing array.
pub(super) struct RowVersionPool<T> {
    slots: FixedVec<T>,
    free: Option<usize>,
}

impl<T> RowVersionPool<T> {
    fn new(budget: &mut Budget, name: &'static str, capacity: usize) -> Result<Self, BudgetError> {
        Ok(Self {
            slots: FixedVec::new(budget, name, capacity)?,
            free: None,
        })
    }

    #[cfg(test)]
    pub(super) fn capacity(&self) -> usize {
        self.slots.capacity()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.slots.len()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

impl<T> Index<usize> for RowVersionPool<T> {
    type Output = T;
    fn index(&self, index: usize) -> &Self::Output {
        &self.slots[index]
    }
}

impl<T> IndexMut<usize> for RowVersionPool<T> {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        &mut self.slots[index]
    }
}

pub(super) struct RowVersionState {
    pub(super) pending_row_versions: RowVersionPool<PendingVersionSlot>,
    pub(super) committed_row_versions: RowVersionPool<CommittedVersionSlot>,
}

pub(super) struct RowVersionPools {
    state: RwLock<RowVersionState>,
}

impl RowVersionPools {
    pub(super) const fn control_bytes() -> usize {
        size_of::<Self>()
    }

    pub(super) fn new(config: &Config, budget: &mut Budget) -> Result<Self, BudgetError> {
        budget.draw(Self::control_bytes(), "row-version pool controls")?;
        Ok(Self {
            state: RwLock::new(RowVersionState {
                pending_row_versions: RowVersionPool::new(
                    budget,
                    "pending_row_versions",
                    pending_row_version_capacity(config),
                )?,
                committed_row_versions: RowVersionPool::new(
                    budget,
                    "committed_row_versions",
                    committed_row_version_capacity(config),
                )?,
            }),
        })
    }

    pub(super) fn read(&self) -> RwLockReadGuard<'_, RowVersionState> {
        self.state.read().expect("row-version pool lock poisoned")
    }

    #[cfg(test)]
    pub(super) fn test_write(
        &self,
    ) -> std::sync::TryLockResult<std::sync::RwLockWriteGuard<'_, RowVersionState>> {
        self.state.try_write()
    }

    /// Exclusive storage ownership excludes shared readers without reacquiring
    /// a lock while rollback, compaction, or publication updates both chains.
    pub(super) fn exclusive(&mut self) -> &mut RowVersionState {
        self.state
            .get_mut()
            .expect("row-version pool lock poisoned")
    }
}

pub(super) fn pending_last(
    pool: &RowVersionPool<PendingVersionSlot>,
    versions: PendingVersions,
) -> Option<PendingChange> {
    versions.tail.map(|slot| pool[slot].change)
}

pub(super) fn pending_visible_at(
    pool: &RowVersionPool<PendingVersionSlot>,
    versions: PendingVersions,
    txid: u32,
    snapshot: u32,
) -> Option<Option<RowLoc>> {
    let mut slot = versions.tail;
    while let Some(index) = slot {
        let entry = &pool[index];
        if entry.change.txid == txid && entry.change.cid < snapshot {
            return Some(entry.change.loc);
        }
        slot = entry.previous;
    }
    None
}

pub(super) fn push_pending_version(
    pool: &mut RowVersionPool<PendingVersionSlot>,
    versions: &mut PendingVersions,
    maximum: usize,
    change: PendingChange,
) -> Result<(), SqlError> {
    let (pool, free) = (&mut pool.slots, &mut pool.free);
    if versions.len >= maximum {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "one row exceeds max_row_versions_per_row ({}) pending command versions",
            maximum
        ));
    }
    let previous = versions.tail;
    let slot = match free.take() {
        Some(slot) => {
            debug_assert!(!pool[slot].used);
            *free = pool[slot].previous;
            pool[slot] = PendingVersionSlot {
                used: true,
                previous,
                change,
            };
            slot
        }
        None => {
            let slot = pool.len();
            pool.push(PendingVersionSlot {
                used: true,
                previous,
                change,
            })
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "pending row-version pool is exhausted"
                )
            })?;
            slot
        }
    };
    versions.tail = Some(slot);
    versions.len += 1;
    Ok(())
}

pub(super) fn pop_pending_version(
    pool: &mut RowVersionPool<PendingVersionSlot>,
    versions: &mut PendingVersions,
) -> Option<PendingChange> {
    let (pool, free) = (&mut pool.slots, &mut pool.free);
    let slot = versions.tail?;
    let entry = pool[slot];
    debug_assert!(entry.used);
    pool[slot].used = false;
    pool[slot].previous = *free;
    *free = Some(slot);
    versions.tail = entry.previous;
    versions.len -= 1;
    Some(entry.change)
}

pub(super) fn clear_pending_versions(
    pool: &mut RowVersionPool<PendingVersionSlot>,
    versions: &mut PendingVersions,
) {
    while pop_pending_version(pool, versions).is_some() {}
}

pub(super) fn release_pending_chain(
    pool: &mut RowVersionPool<PendingVersionSlot>,
    mut slot: Option<usize>,
) {
    let (pool, free) = (&mut pool.slots, &mut pool.free);
    while let Some(index) = slot {
        let entry = pool[index];
        debug_assert!(entry.used);
        pool[index].used = false;
        pool[index].previous = *free;
        *free = Some(index);
        slot = entry.previous;
    }
}

pub(super) fn committed_history_get(
    pool: &RowVersionPool<CommittedVersionSlot>,
    history: CommittedHistory,
    index: usize,
) -> Option<CommittedVersion> {
    if index >= history.len() {
        return None;
    }
    let mut slot = history.tail;
    for _ in 0..index {
        slot = pool[slot?].previous;
    }
    slot.map(|slot| pool[slot].version)
}

pub(super) fn committed_visible_at(
    pool: &RowVersionPool<CommittedVersionSlot>,
    history: CommittedHistory,
    commit_snapshot: u64,
) -> Option<Option<RowHome>> {
    let mut slot = history.tail;
    while let Some(index) = slot {
        let entry = &pool[index];
        if entry.version.lsn <= commit_snapshot {
            return Some(entry.version.home);
        }
        slot = entry.previous;
    }
    None
}

pub(super) fn push_committed_version(
    pool: &mut RowVersionPool<CommittedVersionSlot>,
    history: &mut CommittedHistory,
    maximum: usize,
    version: CommittedVersion,
) -> Result<(), SqlError> {
    let (pool, free) = (&mut pool.slots, &mut pool.free);
    if history.len >= maximum {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "one row exceeds max_row_versions_per_row ({}) committed snapshot versions",
            maximum
        ));
    }
    let previous = history.tail;
    let slot = match free.take() {
        Some(slot) => {
            debug_assert!(!pool[slot].used);
            *free = pool[slot].previous;
            pool[slot] = CommittedVersionSlot {
                used: true,
                previous,
                version,
            };
            slot
        }
        None => {
            let slot = pool.len();
            pool.push(CommittedVersionSlot {
                used: true,
                previous,
                version,
            })
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "committed row-version pool is exhausted"
                )
            })?;
            slot
        }
    };
    history.tail = Some(slot);
    history.len += 1;
    Ok(())
}

pub(super) fn release_committed_chain(
    pool: &mut RowVersionPool<CommittedVersionSlot>,
    mut slot: Option<usize>,
) {
    let (pool, free) = (&mut pool.slots, &mut pool.free);
    while let Some(index) = slot {
        let entry = pool[index];
        debug_assert!(entry.used);
        pool[index].used = false;
        pool[index].previous = *free;
        *free = Some(index);
        slot = entry.previous;
    }
}

/// Keeps every version newer than the oldest snapshot and the first version
/// at or before it. That is the minimal chain that can answer every active
/// snapshot.
pub(super) fn prune_committed_history(
    pool: &mut RowVersionPool<CommittedVersionSlot>,
    history: &mut CommittedHistory,
    oldest_snapshot: Option<u64>,
) {
    let Some(oldest) = oldest_snapshot else {
        release_committed_chain(pool, history.tail.take());
        history.len = 0;
        return;
    };
    let mut slot = history.tail;
    let mut retained = 0usize;
    while let Some(index) = slot {
        retained += 1;
        let entry = pool[index];
        if entry.version.lsn <= oldest {
            pool[index].previous = None;
            release_committed_chain(pool, entry.previous);
            history.len = retained;
            return;
        }
        slot = entry.previous;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::ColumnSet;

    fn pools(capacity: usize) -> RowVersionPools {
        let mut budget = Budget::new(
            capacity * (size_of::<PendingVersionSlot>() + size_of::<CommittedVersionSlot>()),
        );
        RowVersionPools {
            state: RwLock::new(RowVersionState {
                pending_row_versions: RowVersionPool::new(&mut budget, "test pending", capacity)
                    .unwrap(),
                committed_row_versions: RowVersionPool::new(&mut budget, "test history", capacity)
                    .unwrap(),
            }),
        }
    }

    fn pending(command: u32) -> PendingChange {
        PendingChange {
            txid: 7,
            cid: command,
            loc: Some(RowLoc {
                offset: command,
                len: 4,
            }),
            changed_columns: ColumnSet::EMPTY,
            changes_existence: false,
        }
    }

    fn committed(lsn: u64) -> CommittedVersion {
        CommittedVersion {
            home: Some(RowHome::Heap(RowLoc {
                offset: lsn as u32,
                len: 4,
            })),
            lsn,
        }
    }

    #[test]
    fn row_version_ownership_guard_retains_chains_and_detached_results() {
        let mut pools = pools(2);
        let mut commands = PendingVersions::empty();
        let mut history = CommittedHistory::empty();
        let state = pools.exclusive();
        push_pending_version(
            &mut state.pending_row_versions,
            &mut commands,
            2,
            pending(1),
        )
        .unwrap();
        push_committed_version(
            &mut state.committed_row_versions,
            &mut history,
            2,
            committed(3),
        )
        .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let reader = pools.read();
            assert!(pools.state.try_write().is_err());
            let retained_pending = pending_last(&reader.pending_row_versions, commands).unwrap();
            let retained_history =
                committed_history_get(&reader.committed_row_versions, history, 0).unwrap();
            assert_eq!(
                pending_visible_at(&reader.pending_row_versions, commands, 7, 2),
                Some(retained_pending.loc)
            );
            assert_eq!(
                committed_visible_at(&reader.committed_row_versions, history, 3),
                Some(retained_history.home)
            );
            drop(reader);
            let mut writer = pools.state.try_write().unwrap();
            writer.pending_row_versions[commands.tail.unwrap()].change = pending(9);
            writer.committed_row_versions[history.tail.unwrap()].version = committed(10);
            assert_eq!(retained_pending.cid, 1);
            assert_eq!(retained_history.lsn, 3);
        });
    }

    #[test]
    fn row_version_ownership_exhaustion_rollback_and_pruning_preserve_chains() {
        let mut pools = pools(2);
        let state = pools.exclusive();
        let mut commands = PendingVersions::empty();
        let mut history = CommittedHistory::empty();
        crate::mem::guard::forbid_alloc(|| {
            for command in 1..=2 {
                push_pending_version(
                    &mut state.pending_row_versions,
                    &mut commands,
                    3,
                    pending(command),
                )
                .unwrap();
                push_committed_version(
                    &mut state.committed_row_versions,
                    &mut history,
                    3,
                    committed(u64::from(command)),
                )
                .unwrap();
            }
            let pending_before = commands;
            let history_before = history;
            let pending_error = push_pending_version(
                &mut state.pending_row_versions,
                &mut commands,
                3,
                pending(3),
            )
            .unwrap_err();
            let history_error = push_committed_version(
                &mut state.committed_row_versions,
                &mut history,
                3,
                committed(3),
            )
            .unwrap_err();
            assert_eq!(
                pending_error.sqlstate,
                crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(
                history_error.sqlstate,
                crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(
                pending_error.message.as_str(),
                "pending row-version pool is exhausted"
            );
            assert_eq!(
                history_error.message.as_str(),
                "committed row-version pool is exhausted"
            );
            assert_eq!(commands, pending_before);
            assert_eq!(history, history_before);
            assert_eq!(
                pop_pending_version(&mut state.pending_row_versions, &mut commands)
                    .unwrap()
                    .cid,
                2
            );
            assert_eq!(
                pending_visible_at(&state.pending_row_versions, commands, 7, 2),
                Some(pending(1).loc)
            );
            push_pending_version(
                &mut state.pending_row_versions,
                &mut commands,
                3,
                pending(4),
            )
            .unwrap();
            assert_eq!(state.pending_row_versions.len(), 2);
            prune_committed_history(&mut state.committed_row_versions, &mut history, Some(2));
            assert_eq!(history.len(), 1);
            assert_eq!(
                committed_history_get(&state.committed_row_versions, history, 0)
                    .unwrap()
                    .lsn,
                2
            );
            push_committed_version(
                &mut state.committed_row_versions,
                &mut history,
                3,
                committed(5),
            )
            .unwrap();
            assert_eq!(state.committed_row_versions.len(), 2);
            clear_pending_versions(&mut state.pending_row_versions, &mut commands);
            prune_committed_history(&mut state.committed_row_versions, &mut history, None);
            assert!(commands.is_none());
            assert!(history.is_empty());
            for epoch in 6..106 {
                push_pending_version(
                    &mut state.pending_row_versions,
                    &mut commands,
                    2,
                    pending(epoch),
                )
                .unwrap();
                push_committed_version(
                    &mut state.committed_row_versions,
                    &mut history,
                    2,
                    committed(u64::from(epoch)),
                )
                .unwrap();
                clear_pending_versions(&mut state.pending_row_versions, &mut commands);
                prune_committed_history(&mut state.committed_row_versions, &mut history, None);
            }
            assert_eq!(state.pending_row_versions.len(), 2);
            assert_eq!(state.committed_row_versions.len(), 2);
        });
    }

    #[test]
    fn row_version_ownership_readers_observe_both_pools_at_one_epoch() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RowVersionPools>();
        let mut pools = pools(1);
        let mut commands = PendingVersions::empty();
        let mut history = CommittedHistory::empty();
        let state = pools.exclusive();
        push_pending_version(
            &mut state.pending_row_versions,
            &mut commands,
            1,
            pending(0),
        )
        .unwrap();
        push_committed_version(
            &mut state.committed_row_versions,
            &mut history,
            1,
            committed(0),
        )
        .unwrap();
        let barrier = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            for _ in 0..3 {
                let pools = &pools;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    crate::mem::guard::forbid_alloc(|| {
                        for _ in 0..500 {
                            let reader = pools.read();
                            let command =
                                pending_last(&reader.pending_row_versions, commands).unwrap();
                            let version =
                                committed_history_get(&reader.committed_row_versions, history, 0)
                                    .unwrap();
                            assert_eq!(u64::from(command.cid), version.lsn);
                            assert_eq!(command.loc, version.home.and_then(RowHome::heap_loc));
                        }
                    });
                });
            }
            barrier.wait();
            crate::mem::guard::forbid_alloc(|| {
                for epoch in 1..=500 {
                    let mut writer = pools.state.write().unwrap();
                    writer.pending_row_versions[commands.tail.unwrap()].change = pending(epoch);
                    writer.committed_row_versions[history.tail.unwrap()].version =
                        committed(u64::from(epoch));
                }
            });
        });
    }

    #[test]
    fn row_version_ownership_controls_and_arrays_are_charged_exactly_at_startup() {
        let mut config = Config::default_dev();
        config.max_connections = 1;
        config.max_prepared_transactions = 0;
        config.txn_rows = 3;
        config.max_tables = 1;
        config.table_rows = 3;
        config.large_object_pages = 1;
        config.max_row_versions_per_row = 2;
        let bytes = RowVersionPools::control_bytes()
            + pending_row_version_capacity(&config) * size_of::<PendingVersionSlot>()
            + committed_row_version_capacity(&config) * size_of::<CommittedVersionSlot>();
        let mut budget = Budget::new(bytes);
        let pools = RowVersionPools::new(&config, &mut budget).unwrap();
        assert_eq!(budget.remaining(), 0);
        let reader = pools.read();
        assert_eq!(
            reader.pending_row_versions.capacity(),
            pending_row_version_capacity(&config)
        );
        assert_eq!(
            reader.committed_row_versions.capacity(),
            committed_row_version_capacity(&config)
        );
        assert!(reader.pending_row_versions.is_empty());
        assert!(reader.committed_row_versions.is_empty());
        assert_eq!(reader.pending_row_versions.free, None);
        assert_eq!(reader.committed_row_versions.free, None);
        let mut short = Budget::new(bytes - 1);
        assert!(RowVersionPools::new(&config, &mut short).is_err());
    }
}
