//! One ownership boundary for row-version arrays and their free lists.

use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use core::mem::size_of;
use core::ops::{Index, IndexMut};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use super::{
    CommittedHistory, CommittedVersion, CommittedVersionSlot, PendingChange, PendingVersionSlot,
    PendingVersions, RowHome, RowLoc, RowState, committed_row_version_capacity,
    pending_row_version_capacity,
};
use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;

/// The allocator's reusable slots always belong to its own backing array.
pub(super) struct RowVersionPool<T> {
    slots: FixedVec<T>,
    free: Option<usize>,
    next_identity: u64,
}

impl<T> RowVersionPool<T> {
    fn new(budget: &mut Budget, name: &'static str, capacity: usize) -> Result<Self, BudgetError> {
        Ok(Self {
            slots: FixedVec::new(budget, name, capacity)?,
            free: None,
            next_identity: 0,
        })
    }

    #[cfg(test)]
    pub(super) fn capacity(&self) -> usize {
        self.slots.capacity()
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
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
    pub(super) history_pressure_changed: bool,
}

impl RowVersionState {
    pub(super) fn retain_current_row(
        &mut self,
        row: &mut RowState,
        maximum: usize,
    ) -> Result<(), SqlError> {
        let reservation = match row.pending.history_reservation {
            PendingHistory::Reserved(slot) => slot,
            PendingHistory::Published => return Ok(()),
            PendingHistory::None => {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "pending row has no owned history reservation"
                ));
            }
        };
        publish_reserved_history(
            &mut self.committed_row_versions,
            &mut row.history,
            maximum,
            reservation,
            CommittedVersion {
                home: row.committed,
                lsn: row.committed_lsn,
            },
        );
        row.pending.history_reservation = PendingHistory::Published;
        if row.history.len().saturating_add(2) >= maximum {
            self.history_pressure_changed = true;
        }
        Ok(())
    }
}

pub(super) struct RowVersionPools {
    state: RwLock<RowVersionState>,
    pub(super) history_pressure: std::sync::atomic::AtomicBool,
    pub(super) metadata: std::sync::Arc<super::row_reads::RowMetadataPool>,
}

impl RowVersionPools {
    pub(super) const fn control_bytes() -> usize {
        size_of::<Self>()
    }

    pub(super) fn metadata_budget_bytes(config: &Config) -> usize {
        super::row_reads::RowMetadataPool::allocation_bytes(config)
    }

    pub(super) fn new(config: &Config, budget: &mut Budget) -> Result<Self, BudgetError> {
        budget.draw(Self::control_bytes(), "row-version pool controls")?;
        Ok(Self {
            history_pressure: std::sync::atomic::AtomicBool::new(false),
            metadata: super::row_reads::RowMetadataPool::new(config, budget)?,
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
                history_pressure_changed: false,
            }),
        })
    }

    pub(super) fn try_read_for_batch(
        &self,
    ) -> Result<RwLockReadGuard<'_, RowVersionState>, super::row_ownership::LockWait<'_>> {
        super::row_ownership::try_read(&self.state, "row-version pool lock poisoned")
    }

    pub(super) fn try_write_for_batch(
        &self,
    ) -> Result<RwLockWriteGuard<'_, RowVersionState>, super::row_ownership::LockWait<'_>> {
        super::row_ownership::try_write(&self.state, "row-version pool lock poisoned")
    }

    #[cfg(test)]
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

impl super::Storage {
    /// Recovered prepared transactions replay catalog operations after undoing
    /// their startup overlays. Retain old row images before that undo so live
    /// snapshots survive the same replay of inserts, updates and deletes.
    pub(crate) fn retain_prepared_row_history(
        &self,
        table: usize,
        rowid: u64,
        transaction: u32,
    ) -> Result<(), SqlError> {
        if self.transaction_mode == crate::config::TransactionMode::Locking
            || self.pending_table_rows_rewritten(table, transaction)
        {
            return Ok(());
        }
        let (snapshots, mut versions, mut rows) = super::row_ownership::acquire(|| {
            Ok((
                super::row_ownership::try_mutex(&self.snapshots, "snapshot state lock poisoned")?,
                self.row_versions.try_write_for_batch()?,
                self.tables[table].rows.try_write_for_batch()?,
            ))
        });
        if !snapshots.retains_for(transaction) {
            return Ok(());
        }
        let Some(row) = rows.get_mut(&rowid) else {
            return Ok(());
        };
        if !row.has_committed_version()
            || pending_last(&versions.pending_row_versions, row.pending)
                .is_none_or(|pending| pending.txid != transaction)
        {
            return Ok(());
        }
        versions.retain_current_row(row, self.max_row_versions_per_row)?;
        if versions.history_pressure_changed {
            self.row_versions
                .history_pressure
                .store(true, std::sync::atomic::Ordering::Release);
        }
        Ok(())
    }
}

pub(super) fn pending_last(
    pool: &RowVersionPool<PendingVersionSlot>,
    versions: PendingVersions,
) -> Option<PendingChange> {
    versions.tail.map(|slot| pool[slot].change)
}

pub(super) fn pending_version_at(
    pool: &RowVersionPool<PendingVersionSlot>,
    versions: PendingVersions,
    txid: u32,
    snapshot: u32,
) -> Option<(u64, Option<RowLoc>)> {
    let mut slot = versions.tail;
    while let Some(index) = slot {
        let entry = &pool[index];
        if entry.change.txid == txid && entry.change.cid < snapshot {
            return Some((entry.identity, entry.change.loc));
        }
        slot = entry.previous;
    }
    None
}

pub(super) fn pending_visible_at(
    pool: &RowVersionPool<PendingVersionSlot>,
    versions: PendingVersions,
    txid: u32,
    snapshot: u32,
) -> Option<Option<RowLoc>> {
    pending_version_at(pool, versions, txid, snapshot).map(|(_, home)| home)
}

pub(super) fn retained_pending_home(
    pool: &RowVersionPool<PendingVersionSlot>,
    versions: PendingVersions,
    identity: u64,
) -> Option<Option<RowLoc>> {
    let mut slot = versions.tail;
    while let Some(index) = slot {
        let entry = &pool[index];
        if entry.identity == identity {
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
) -> Result<u64, SqlError> {
    let identity = pool.next_identity.checked_add(1).ok_or_else(|| {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "pending row-version identity space is exhausted"
        )
    })?;
    let (pool, free, next_identity) = (&mut pool.slots, &mut pool.free, &mut pool.next_identity);
    if versions.len >= maximum {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "one row exceeds max_row_versions_per_row ({}) pending write versions",
            maximum
        ));
    }
    let previous = versions.tail;
    let slot = match free.take() {
        Some(slot) => {
            debug_assert!(!pool[slot].used);
            *free = pool[slot].previous;
            pool[slot] = PendingVersionSlot {
                identity,
                used: true,
                previous,
                change,
            };
            slot
        }
        None => {
            let slot = pool.len();
            pool.push(PendingVersionSlot {
                identity,
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
    *next_identity = identity;
    Ok(identity)
}

pub(super) fn pop_pending_version(
    state: &mut RowVersionState,
    versions: &mut PendingVersions,
) -> Option<PendingChange> {
    let (pool, free) = (
        &mut state.pending_row_versions.slots,
        &mut state.pending_row_versions.free,
    );
    let slot = versions.tail?;
    let entry = pool[slot];
    debug_assert!(entry.used);
    pool[slot].used = false;
    pool[slot].previous = *free;
    *free = Some(slot);
    versions.tail = entry.previous;
    versions.len -= 1;
    if versions.is_none()
        && let PendingHistory::Reserved(reservation) =
            core::mem::replace(&mut versions.history_reservation, PendingHistory::None)
    {
        release_reserved_history(&mut state.committed_row_versions, reservation);
    }
    Some(entry.change)
}

pub(super) fn clear_pending_versions(state: &mut RowVersionState, versions: &mut PendingVersions) {
    while pop_pending_version(state, versions).is_some() {}
}

pub(super) fn release_pending_chain(state: &mut RowVersionState, versions: PendingVersions) {
    if let PendingHistory::Reserved(reservation) = versions.history_reservation {
        release_reserved_history(&mut state.committed_row_versions, reservation);
    }
    let (pool, free) = (
        &mut state.pending_row_versions.slots,
        &mut state.pending_row_versions.free,
    );
    let mut slot = versions.tail;
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

pub(super) fn committed_version_at(
    pool: &RowVersionPool<CommittedVersionSlot>,
    history: CommittedHistory,
    commit_snapshot: u64,
) -> Option<CommittedVersion> {
    let mut slot = history.tail;
    while let Some(index) = slot {
        let entry = &pool[index];
        if entry.version.lsn <= commit_snapshot {
            return Some(entry.version);
        }
        slot = entry.previous;
    }
    None
}

/// This slot is occupied but not linked into visible history until publication.
/// Its pending row chain owns it through staging, PREPARE, and rollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ReservedHistorySlot(usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PendingHistory {
    None,
    Reserved(ReservedHistorySlot),
    /// A recovered prepared commit retained the old image before replay.
    Published,
}

fn allocate_committed_slot(
    pool: &mut RowVersionPool<CommittedVersionSlot>,
    previous: Option<usize>,
    version: CommittedVersion,
) -> Result<usize, SqlError> {
    let (pool, free) = (&mut pool.slots, &mut pool.free);
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
                    "row_history_slots exhausted ({} slots)",
                    pool.capacity()
                )
            })?;
            slot
        }
    };
    Ok(slot)
}

pub(super) fn reserve_history_slot(
    pool: &mut RowVersionPool<CommittedVersionSlot>,
) -> Result<ReservedHistorySlot, SqlError> {
    // Reserving capacity retains no extra heap location. Publication fills the
    // image from the then-current row, including any intervening relocation.
    allocate_committed_slot(pool, None, CommittedVersion { home: None, lsn: 0 })
        .map(ReservedHistorySlot)
}

fn release_reserved_history(
    pool: &mut RowVersionPool<CommittedVersionSlot>,
    reservation: ReservedHistorySlot,
) {
    let slot = reservation.0;
    assert!(pool[slot].used);
    assert_eq!(pool[slot].version, CommittedVersion { home: None, lsn: 0 });
    pool[slot].used = false;
    pool[slot].previous = pool.free;
    pool.free = Some(slot);
}

pub(super) fn publish_reserved_history(
    pool: &mut RowVersionPool<CommittedVersionSlot>,
    history: &mut CommittedHistory,
    maximum: usize,
    reservation: ReservedHistorySlot,
    version: CommittedVersion,
) {
    assert!(
        history.len < maximum,
        "row history was validated before durability"
    );
    let slot = reservation.0;
    assert!(pool[slot].used);
    assert_eq!(pool[slot].version, CommittedVersion { home: None, lsn: 0 });
    pool[slot].version = version;
    pool[slot].previous = history.tail;
    history.tail = Some(slot);
    history.len += 1;
}

pub(super) fn validate_history_length(
    history: &CommittedHistory,
    maximum: usize,
) -> Result<(), SqlError> {
    if history.len >= maximum {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "one row exceeds max_row_versions_per_row ({}) committed snapshot versions",
            maximum
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn push_committed_version(
    pool: &mut RowVersionPool<CommittedVersionSlot>,
    history: &mut CommittedHistory,
    maximum: usize,
    version: CommittedVersion,
) -> Result<(), SqlError> {
    validate_history_length(history, maximum)?;
    let slot = allocate_committed_slot(pool, history.tail, version)?;
    history.tail = Some(slot);
    history.len += 1;
    Ok(())
}

pub(super) fn release_committed_chain(
    pool: &mut RowVersionPool<CommittedVersionSlot>,
    mut slot: Option<usize>,
) -> bool {
    let (pool, free) = (&mut pool.slots, &mut pool.free);
    let mut heap_changed = false;
    while let Some(index) = slot {
        let entry = pool[index];
        debug_assert!(entry.used);
        heap_changed |= matches!(entry.version.home, Some(RowHome::Heap(_)));
        pool[index].used = false;
        pool[index].previous = *free;
        *free = Some(index);
        slot = entry.previous;
    }
    heap_changed
}

/// Keep the oldest reader's boundary and every newer image. If the current
/// value or deletion covers all readers, no history remains necessary.
/// Reports whether any heap reference was released.
pub(super) fn prune_committed_history(
    pool: &mut RowVersionPool<CommittedVersionSlot>,
    row: &mut RowState,
    oldest_snapshot: Option<u64>,
) -> bool {
    let oldest = oldest_snapshot
        .filter(|snapshot| !row.has_committed_version() || *snapshot < row.committed_lsn);
    prune_committed_chain(pool, &mut row.history, oldest)
}

pub(super) fn prune_committed_chain(
    pool: &mut RowVersionPool<CommittedVersionSlot>,
    history: &mut CommittedHistory,
    oldest_snapshot: Option<u64>,
) -> bool {
    let Some(oldest) = oldest_snapshot else {
        let heap_changed = release_committed_chain(pool, history.tail.take());
        history.len = 0;
        return heap_changed;
    };
    let mut slot = history.tail;
    let mut retained = 0usize;
    while let Some(index) = slot {
        retained += 1;
        let entry = pool[index];
        if entry.version.lsn <= oldest {
            pool[index].previous = None;
            let heap_changed = release_committed_chain(pool, entry.previous);
            history.len = retained;
            return heap_changed;
        }
        slot = entry.previous;
    }
    false
}

/// Published roots reproduce the prefix; reports released heap references.
pub(super) fn release_committed_history_through(
    pool: &mut RowVersionPool<CommittedVersionSlot>,
    history: &mut CommittedHistory,
    through_lsn: u64,
) -> bool {
    let mut slot = history.tail;
    let mut previous = None;
    let mut retained = 0;
    while let Some(index) = slot {
        let entry = pool[index];
        if entry.version.lsn <= through_lsn {
            if let Some(previous) = previous {
                pool[previous].previous = None;
            } else {
                history.tail = None;
            }
            let heap_changed = release_committed_chain(pool, Some(index));
            history.len = retained;
            return heap_changed;
        }
        retained += 1;
        previous = Some(index);
        slot = entry.previous;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{ColumnSet, RowHome};

    fn pools(capacity: usize) -> RowVersionPools {
        let mut config = Config::default_dev();
        config.max_row_versions_per_row = capacity;
        let mut budget = Budget::new(
            capacity * (size_of::<PendingVersionSlot>() + size_of::<CommittedVersionSlot>())
                + RowVersionPools::metadata_budget_bytes(&config),
        );
        RowVersionPools {
            history_pressure: std::sync::atomic::AtomicBool::new(false),
            metadata: super::super::row_reads::RowMetadataPool::new(&config, &mut budget).unwrap(),
            state: RwLock::new(RowVersionState {
                pending_row_versions: RowVersionPool::new(&mut budget, "test pending", capacity)
                    .unwrap(),
                committed_row_versions: RowVersionPool::new(&mut budget, "test history", capacity)
                    .unwrap(),
                history_pressure_changed: false,
            }),
        }
    }

    #[test]
    fn row_history_reservation_survives_competition_and_savepoint_undo() {
        let mut pools = pools(2);
        let state = pools.exclusive();
        let mut commands = PendingVersions::empty();
        let mut history = CommittedHistory::empty();
        crate::mem::guard::forbid_alloc(|| {
            push_pending_version(
                &mut state.pending_row_versions,
                &mut commands,
                2,
                pending(1),
            )
            .unwrap();
            commands.history_reservation = PendingHistory::Reserved(
                reserve_history_slot(&mut state.committed_row_versions).unwrap(),
            );
            push_pending_version(
                &mut state.pending_row_versions,
                &mut commands,
                2,
                pending(2),
            )
            .unwrap();
            let competing = reserve_history_slot(&mut state.committed_row_versions).unwrap();
            assert!(reserve_history_slot(&mut state.committed_row_versions).is_err());
            assert!(
                committed_version_at(&state.committed_row_versions, history, u64::MAX).is_none()
            );
            pop_pending_version(state, &mut commands).unwrap();
            assert!(
                reserve_history_slot(&mut state.committed_row_versions).is_err(),
                "rolling back one command must retain the first write's reservation"
            );
            pop_pending_version(state, &mut commands).unwrap();
            assert_eq!(commands.history_reservation, PendingHistory::None);
            let reused = reserve_history_slot(&mut state.committed_row_versions).unwrap();
            publish_reserved_history(
                &mut state.committed_row_versions,
                &mut history,
                2,
                competing,
                committed(7),
            );
            assert_eq!(
                committed_version_at(&state.committed_row_versions, history, 7),
                Some(committed(7))
            );
            assert!(
                reserve_history_slot(&mut state.committed_row_versions).is_err(),
                "publication must consume its own slot, even while another writer owns the rest"
            );
            release_reserved_history(&mut state.committed_row_versions, reused);
            release_committed_chain(&mut state.committed_row_versions, history.tail);
            let first = reserve_history_slot(&mut state.committed_row_versions).unwrap();
            let second = reserve_history_slot(&mut state.committed_row_versions).unwrap();
            release_reserved_history(&mut state.committed_row_versions, first);
            release_reserved_history(&mut state.committed_row_versions, second);
        });
    }

    #[test]
    fn row_history_reservation_reclaims_images_when_the_current_row_covers_readers() {
        let mut pools = pools(2);
        let state = pools.exclusive();
        let mut row = RowState::committed_only_at(RowLoc::test(3, 4), 3);
        crate::mem::guard::forbid_alloc(|| {
            for lsn in [1, 2] {
                push_committed_version(
                    &mut state.committed_row_versions,
                    &mut row.history,
                    2,
                    committed(lsn),
                )
                .unwrap();
            }
            assert!(prune_committed_history(
                &mut state.committed_row_versions,
                &mut row,
                Some(2)
            ));
            assert_eq!(row.history.len(), 1);
            assert_eq!(
                committed_version_at(&state.committed_row_versions, row.history, 2),
                Some(committed(2))
            );
            assert!(prune_committed_history(
                &mut state.committed_row_versions,
                &mut row,
                Some(3)
            ));
            assert!(row.history.is_empty());
            let first = reserve_history_slot(&mut state.committed_row_versions).unwrap();
            let second = reserve_history_slot(&mut state.committed_row_versions).unwrap();
            release_reserved_history(&mut state.committed_row_versions, first);
            release_reserved_history(&mut state.committed_row_versions, second);
            row.committed = None;
            push_committed_version(
                &mut state.committed_row_versions,
                &mut row.history,
                2,
                committed(2),
            )
            .unwrap();
            assert!(prune_committed_history(
                &mut state.committed_row_versions,
                &mut row,
                Some(3)
            ));
            assert!(
                row.history.is_empty(),
                "a visible deletion also covers newer readers"
            );
        });
    }

    fn pending(command: u32) -> PendingChange {
        PendingChange {
            txid: 7,
            cid: command,
            loc: Some(RowLoc::test(command, 4)),
            changed_columns: ColumnSet::EMPTY,
            changes_existence: false,
        }
    }

    fn committed(lsn: u64) -> CommittedVersion {
        CommittedVersion {
            home: Some(RowHome::Heap(RowLoc::test(lsn as u32, 4))),
            lsn,
        }
    }

    #[test]
    fn visible_row_heap_ownership_pending_identity_exhaustion_preserves_pool() {
        let mut owner = pools(1);
        let state = owner.exclusive();
        let mut versions = PendingVersions::empty();
        push_pending_version(
            &mut state.pending_row_versions,
            &mut versions,
            1,
            pending(1),
        )
        .unwrap();
        pop_pending_version(state, &mut versions).unwrap();
        let free = state.pending_row_versions.free;
        state.pending_row_versions.next_identity = u64::MAX;
        crate::mem::guard::forbid_alloc(|| {
            let error = push_pending_version(
                &mut state.pending_row_versions,
                &mut versions,
                1,
                pending(1),
            )
            .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(state.pending_row_versions.free, free);
            assert_eq!(state.pending_row_versions.len(), 1);
            assert_eq!(state.pending_row_versions.next_identity, u64::MAX);
            assert!(versions.is_none());
        });
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
                committed_version_at(&reader.committed_row_versions, history, 3)
                    .map(|version| version.home),
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
                "row_history_slots exhausted (2 slots)"
            );
            assert_eq!(commands, pending_before);
            assert_eq!(history, history_before);
            assert_eq!(pop_pending_version(state, &mut commands).unwrap().cid, 2);
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
            assert!(prune_committed_chain(
                &mut state.committed_row_versions,
                &mut history,
                Some(2)
            ));
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
            clear_pending_versions(state, &mut commands);
            assert!(prune_committed_chain(
                &mut state.committed_row_versions,
                &mut history,
                None
            ));
            assert!(commands.is_none());
            assert!(history.is_empty());
            assert!(!prune_committed_chain(
                &mut state.committed_row_versions,
                &mut history,
                None
            ));
            let saved = CommittedVersion {
                home: Some(RowHome::Spilled {
                    len: 4,
                    sst: crate::storage::SpillGenerationId::fixture(0),
                    commit_lsn: 5,
                }),
                lsn: 5,
            };
            push_committed_version(&mut state.committed_row_versions, &mut history, 3, saved)
                .unwrap();
            assert!(!release_committed_history_through(
                &mut state.committed_row_versions,
                &mut history,
                5
            ));
            push_committed_version(
                &mut state.committed_row_versions,
                &mut history,
                3,
                committed(5),
            )
            .unwrap();
            assert!(release_committed_history_through(
                &mut state.committed_row_versions,
                &mut history,
                5
            ));
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
                clear_pending_versions(state, &mut commands);
                prune_committed_chain(&mut state.committed_row_versions, &mut history, None);
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
        config.row_history_slots = 8;
        let bytes = RowVersionPools::control_bytes()
            + RowVersionPools::metadata_budget_bytes(&config)
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
