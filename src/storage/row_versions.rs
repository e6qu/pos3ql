//! One ownership boundary for row-version arrays and their free lists.

use core::mem::size_of;
use std::sync::{RwLock, RwLockReadGuard};

use super::{
    CommittedVersionSlot, PendingVersionSlot, committed_row_version_capacity,
    pending_row_version_capacity,
};
use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;

pub(super) struct RowVersionState {
    pub(super) pending_row_versions: FixedVec<PendingVersionSlot>,
    pub(super) pending_row_version_free: Option<usize>,
    pub(super) committed_row_versions: FixedVec<CommittedVersionSlot>,
    pub(super) committed_row_version_free: Option<usize>,
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
                pending_row_versions: FixedVec::new(
                    budget,
                    "pending_row_versions",
                    pending_row_version_capacity(config),
                )?,
                pending_row_version_free: None,
                committed_row_versions: FixedVec::new(
                    budget,
                    "committed_row_versions",
                    committed_row_version_capacity(config),
                )?,
                committed_row_version_free: None,
            }),
        })
    }

    pub(super) fn read(&self) -> RwLockReadGuard<'_, RowVersionState> {
        self.state.read().expect("row-version pool lock poisoned")
    }

    /// Exclusive storage ownership excludes shared readers without reacquiring
    /// a lock while rollback, compaction, or publication updates both chains.
    pub(super) fn exclusive(&mut self) -> &mut RowVersionState {
        self.state.get_mut().expect("row-version pool lock poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{
        ColumnSet, CommittedHistory, CommittedVersion, PendingChange, PendingVersions, RowHome,
        RowLoc, clear_pending_versions, committed_history_get, committed_visible_at, pending_last,
        pending_visible_at, pop_pending_version, prune_committed_history, push_committed_version,
        push_pending_version,
    };

    fn pools(capacity: usize) -> RowVersionPools {
        let mut budget = Budget::new(
            capacity * (size_of::<PendingVersionSlot>() + size_of::<CommittedVersionSlot>()),
        );
        RowVersionPools {
            state: RwLock::new(RowVersionState {
                pending_row_versions: FixedVec::new(&mut budget, "test pending", capacity).unwrap(),
                pending_row_version_free: None,
                committed_row_versions: FixedVec::new(&mut budget, "test history", capacity).unwrap(),
                committed_row_version_free: None,
            }),
        }
    }

    fn pending(command: u32) -> PendingChange {
        PendingChange {
            txid: 7,
            cid: command,
            loc: Some(RowLoc { offset: command, len: 4 }),
            changed_columns: ColumnSet::EMPTY,
            changes_existence: false,
        }
    }

    fn committed(lsn: u64) -> CommittedVersion {
        CommittedVersion {
            home: Some(RowHome::Heap(RowLoc { offset: lsn as u32, len: 4 })),
            lsn,
        }
    }

    #[test]
    fn row_version_ownership_guard_retains_chains_and_detached_results() {
        let mut pools = pools(2);
        let mut commands = PendingVersions::empty();
        let mut history = CommittedHistory::empty();
        let state = pools.exclusive();
        push_pending_version(&mut state.pending_row_versions, &mut state.pending_row_version_free,
            &mut commands, 2, pending(1)).unwrap();
        push_committed_version(&mut state.committed_row_versions, &mut state.committed_row_version_free,
            &mut history, 2, committed(3)).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let reader = pools.read();
            assert!(pools.state.try_write().is_err());
            let retained_pending = pending_last(&reader.pending_row_versions, commands).unwrap();
            let retained_history = committed_history_get(&reader.committed_row_versions, history, 0).unwrap();
            assert_eq!(pending_visible_at(&reader.pending_row_versions, commands, 7, 2), Some(retained_pending.loc));
            assert_eq!(committed_visible_at(&reader.committed_row_versions, history, 3), Some(retained_history.home));
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
                push_pending_version(&mut state.pending_row_versions, &mut state.pending_row_version_free,
                    &mut commands, 3, pending(command)).unwrap();
                push_committed_version(&mut state.committed_row_versions, &mut state.committed_row_version_free,
                    &mut history, 3, committed(u64::from(command))).unwrap();
            }
            let pending_before = commands;
            let history_before = history;
            let pending_error = push_pending_version(&mut state.pending_row_versions, &mut state.pending_row_version_free,
                &mut commands, 3, pending(3)).unwrap_err();
            let history_error = push_committed_version(&mut state.committed_row_versions, &mut state.committed_row_version_free,
                &mut history, 3, committed(3)).unwrap_err();
            assert_eq!(pending_error.sqlstate, crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(history_error.sqlstate, crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(pending_error.message.as_str(), "pending row-version pool is exhausted");
            assert_eq!(history_error.message.as_str(), "committed row-version pool is exhausted");
            assert_eq!(commands, pending_before);
            assert_eq!(history, history_before);
            assert_eq!(pop_pending_version(&mut state.pending_row_versions, &mut state.pending_row_version_free,
                &mut commands).unwrap().cid, 2);
            assert_eq!(pending_visible_at(&state.pending_row_versions, commands, 7, 2), Some(pending(1).loc));
            push_pending_version(&mut state.pending_row_versions, &mut state.pending_row_version_free,
                &mut commands, 3, pending(4)).unwrap();
            assert_eq!(state.pending_row_versions.len(), 2);
            prune_committed_history(&mut state.committed_row_versions, &mut state.committed_row_version_free,
                &mut history, Some(2));
            assert_eq!(history.len(), 1);
            assert_eq!(committed_history_get(&state.committed_row_versions, history, 0).unwrap().lsn, 2);
            push_committed_version(&mut state.committed_row_versions, &mut state.committed_row_version_free,
                &mut history, 3, committed(5)).unwrap();
            assert_eq!(state.committed_row_versions.len(), 2);
            clear_pending_versions(&mut state.pending_row_versions, &mut state.pending_row_version_free, &mut commands);
            prune_committed_history(&mut state.committed_row_versions, &mut state.committed_row_version_free,
                &mut history, None);
            assert!(commands.is_none());
            assert!(history.is_empty());
            for epoch in 6..106 {
                push_pending_version(&mut state.pending_row_versions, &mut state.pending_row_version_free,
                    &mut commands, 2, pending(epoch)).unwrap();
                push_committed_version(&mut state.committed_row_versions, &mut state.committed_row_version_free,
                    &mut history, 2, committed(u64::from(epoch))).unwrap();
                clear_pending_versions(&mut state.pending_row_versions, &mut state.pending_row_version_free, &mut commands);
                prune_committed_history(&mut state.committed_row_versions, &mut state.committed_row_version_free,
                    &mut history, None);
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
        push_pending_version(&mut state.pending_row_versions, &mut state.pending_row_version_free,
            &mut commands, 1, pending(0)).unwrap();
        push_committed_version(&mut state.committed_row_versions, &mut state.committed_row_version_free,
            &mut history, 1, committed(0)).unwrap();
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
                            let command = pending_last(&reader.pending_row_versions, commands).unwrap();
                            let version = committed_history_get(&reader.committed_row_versions, history, 0).unwrap();
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
                    writer.committed_row_versions[history.tail.unwrap()].version = committed(u64::from(epoch));
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
        assert_eq!(reader.pending_row_versions.capacity(), pending_row_version_capacity(&config));
        assert_eq!(reader.committed_row_versions.capacity(), committed_row_version_capacity(&config));
        assert!(reader.pending_row_versions.is_empty());
        assert!(reader.committed_row_versions.is_empty());
        assert_eq!(reader.pending_row_version_free, None);
        assert_eq!(reader.committed_row_version_free, None);
        let mut short = Budget::new(bytes - 1);
        assert!(RowVersionPools::new(&config, &mut short).is_err());
    }
}
