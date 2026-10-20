//! Row images whose chain handles retain the version owner that issued them.

use super::RowEntries;
use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use core::ops::Deref;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard};

use super::row_heap::HeapReadView;
use super::row_images::RowVersionIdentity;
use super::row_map::RowMap;
use super::row_versions::{
    RowVersionPools, RowVersionState, committed_history_get, committed_version_at, pending_last,
    pending_version_at, pending_visible_at, retained_pending_home,
};
use super::spill_generations::{
    CapturedSpillRoots, QueryRootLease, SpilledRowRead, SpilledRowState,
};
use super::{CommittedVersion, PendingChange, RowHome, RowLoc, RowState};

const METADATA_READS_PER_WORKSPACE: usize = 64;

pub(crate) enum RowSelfChange {
    Original,
    SameCommand,
    TriggeredCommand,
}

impl super::Storage {
    /// Only pending metadata is needed; no byte read or callback runs while
    /// definition, version, and row-map guards are held.
    pub(crate) fn row_self_change(
        &self,
        table: usize,
        rowid: u64,
        original: super::RowSnapshot,
        txid: u32,
        command: u32,
    ) -> Result<RowSelfChange, SqlError> {
        let table = self.tables.get(table).ok_or_else(|| {
            sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "selected DML table was removed"
            )
        })?;
        let (definition, versions, rows) = super::row_ownership::acquire(|| {
            Ok((
                table.definition.try_read_for_batch()?,
                self.row_versions.try_read_for_batch()?,
                table.rows.try_read_for_batch()?,
            ))
        });
        let identity = definition.identity;
        if identity.created_at != original.created_at || !identity.existence.visible_to(txid) {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "selected DML table was replaced"
            ));
        }
        let Some(&state) = rows.get(&rowid) else {
            return Ok(RowSelfChange::Original);
        };
        let state = RowRead::borrowed(state, &versions);
        let Some(pending) = state.pending_last().filter(|pending| pending.txid == txid) else {
            return Ok(RowSelfChange::Original);
        };
        let identity = state.pending_head_identity().ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "pending DML row has no version identity"
            )
        })?;
        if original.version == RowVersionIdentity::Pending(identity) {
            return Ok(RowSelfChange::Original);
        }
        Ok(if pending.cid == command {
            RowSelfChange::SameCommand
        } else {
            RowSelfChange::TriggeredCommand
        })
    }
}

pub(super) struct CapturedRowMetadata {
    pending: FixedVec<(u64, PendingChange)>,
    history: FixedVec<CommittedVersion>,
    sources: Option<CapturedRowSources>,
}

pub(super) struct CapturedRowSources {
    pub(super) table: usize,
    pub(super) rowid: u64,
    pub(super) created_at: u64,
    pub(super) heap: Option<HeapReadView>,
    pub(super) objects: Option<QueryRootLease>,
}

struct MetadataSlot {
    retained: AtomicBool,
    image: RwLock<CapturedRowMetadata>,
}

pub(super) struct RowMetadataPool {
    slots: FixedVec<MetadataSlot>,
}

struct MetadataLease {
    pool: Arc<RowMetadataPool>,
    slot: usize,
}

impl MetadataLease {
    fn read(&self) -> RwLockReadGuard<'_, CapturedRowMetadata> {
        self.pool.slots[self.slot]
            .image
            .read()
            .expect("row metadata slot poisoned")
    }
}

impl Drop for MetadataLease {
    fn drop(&mut self) {
        let slot = &self.pool.slots[self.slot];
        {
            let mut image = slot.image.write().expect("row metadata slot poisoned");
            image.pending.clear();
            image.history.clear();
            image.sources = None;
        }
        slot.retained.store(false, Ordering::Release);
    }
}

impl RowMetadataPool {
    pub(super) fn allocation_bytes(config: &Config) -> usize {
        let slots = config
            .query_workspace_slots
            .saturating_mul(METADATA_READS_PER_WORKSPACE);
        (size_of::<Self>() + 2 * size_of::<usize>()).saturating_add(slots.saturating_mul(
            size_of::<MetadataSlot>().saturating_add(
                config.max_row_versions_per_row.saturating_mul(
                    size_of::<(u64, PendingChange)>() + size_of::<CommittedVersion>(),
                ),
            ),
        ))
    }

    pub(super) fn new(config: &Config, budget: &mut Budget) -> Result<Arc<Self>, BudgetError> {
        budget.draw(
            size_of::<Self>() + 2 * size_of::<usize>(),
            "row metadata pool owner",
        )?;
        let count = config
            .query_workspace_slots
            .saturating_mul(METADATA_READS_PER_WORKSPACE);
        let mut slots = FixedVec::new(budget, "row metadata read slots", count)?;
        for _ in 0..count {
            slots
                .push(MetadataSlot {
                    retained: AtomicBool::new(false),
                    image: RwLock::new(CapturedRowMetadata {
                        pending: FixedVec::new(
                            budget,
                            "captured pending row versions",
                            config.max_row_versions_per_row,
                        )?,
                        history: FixedVec::new(
                            budget,
                            "captured committed row versions",
                            config.max_row_versions_per_row,
                        )?,
                        sources: None,
                    }),
                })
                .expect("sized to row metadata read slots");
        }
        Ok(Arc::new(Self { slots }))
    }

    fn capture(
        self: &Arc<Self>,
        versions: &RowVersionState,
        state: RowState,
        sources: impl FnOnce(&CapturedRowMetadata) -> Result<Option<CapturedRowSources>, SqlError>,
    ) -> Result<MetadataLease, SqlError> {
        let workspace = crate::sql::execution_query_workspace().index();
        let slots = workspace
            .checked_mul(METADATA_READS_PER_WORKSPACE)
            .and_then(|start| {
                start
                    .checked_add(METADATA_READS_PER_WORKSPACE)
                    .and_then(|end| self.slots.get(start..end))
            })
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "row metadata workspace is outside its startup reservation"
                )
            })?;
        let local = slots
            .iter()
            .position(|slot| {
                slot.retained
                    .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
            })
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "row metadata read slots exhausted ({})",
                    slots.len()
                )
            })?;
        let lease = MetadataLease {
            pool: Arc::clone(self),
            slot: workspace * METADATA_READS_PER_WORKSPACE + local,
        };
        let mut image = self.slots[lease.slot]
            .image
            .write()
            .expect("row metadata slot poisoned");
        image.pending.clear();
        image.history.clear();
        image.sources = None;
        let capacity = || {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "retained row metadata exceeds max_row_versions_per_row"
            )
        };
        let mut slot = state.pending.tail;
        while let Some(index) = slot {
            let entry = versions.pending_row_versions[index];
            image
                .pending
                .push((entry.identity, entry.change))
                .map_err(|_| capacity())?;
            slot = entry.previous;
        }
        let mut slot = state.history.tail;
        while let Some(index) = slot {
            let entry = versions.committed_row_versions[index];
            image.history.push(entry.version).map_err(|_| capacity())?;
            slot = entry.previous;
        }
        image.sources = sources(&image)?;
        drop(image);
        Ok(lease)
    }
}

impl CapturedRowMetadata {
    pub(super) fn has_heap(&self, state: RowState) -> bool {
        matches!(state.committed, Some(RowHome::Heap(_)))
            || self.pending.iter().any(|(_, change)| change.loc.is_some())
            || self
                .history
                .iter()
                .any(|version| matches!(version.home, Some(RowHome::Heap(_))))
    }
}

/// Point reads own copies of their chain images; coherent whole-map views
/// borrow their owner. Only these owning views can issue resident reads.
pub(crate) struct RowRead<'a> {
    state: RowState,
    versions: VersionOwner<'a>,
    captured_roots: Option<&'a CapturedSpillRoots<'a>>,
}

pub(super) type RowReadVisitor<'a> = dyn for<'row> FnMut(
        u64,
        RowRead<'row>,
    ) -> Result<core::ops::ControlFlow<()>, crate::sql::eval::SqlError>
    + 'a;

enum VersionOwner<'a> {
    Retained(MetadataLease),
    Borrowed(&'a RowVersionState),
    Immutable(SpilledRowState),
}

impl Deref for RowRead<'_> {
    type Target = RowState;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl<'owner> RowRead<'owner> {
    pub(super) fn borrowed(state: RowState, versions: &'owner RowVersionState) -> Self {
        Self {
            state,
            versions: VersionOwner::Borrowed(versions),
            captured_roots: None,
        }
    }
    pub(super) fn has_owned_sources(&self) -> bool {
        match &self.versions {
            VersionOwner::Retained(lease) => lease.read().sources.is_some(),
            VersionOwner::Immutable(_) => true,
            VersionOwner::Borrowed(_) => false,
        }
    }

    pub(crate) fn select_owned(
        &self,
        table: usize,
        rowid: u64,
        transaction: u32,
        command_snapshot: u32,
        commit_snapshot: u64,
    ) -> Result<Option<super::VisibleRowHome>, SqlError> {
        if let VersionOwner::Immutable(source) = &self.versions {
            source.validate_request(table, rowid)?;
            return source
                .visible_at(commit_snapshot)
                .map(|home| home.map(super::VisibleRowHome::spilled));
        }
        let selected = self.visible_version_at(transaction, command_snapshot, commit_snapshot);
        if selected.is_none() {
            return self.lookup_owned_objects(table, rowid, commit_snapshot);
        }
        self.pin_owned(table, rowid, selected)
    }

    pub(super) fn lookup_owned_objects(
        &self,
        table: usize,
        rowid: u64,
        snapshot: u64,
    ) -> Result<Option<super::VisibleRowHome>, SqlError> {
        let VersionOwner::Retained(lease) = &self.versions else {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "row metadata has no owned source"
            ));
        };
        let (created_at, roots) = {
            let image = lease.read();
            let source = image.sources.as_ref().ok_or_else(|| {
                sql_err!(sqlstate::INTERNAL_ERROR, "row metadata has no owned source")
            })?;
            if source.table != table || source.rowid != rowid {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "retained row metadata does not match the requested table and row"
                ));
            }
            (
                source.created_at,
                source
                    .objects
                    .as_ref()
                    .map(QueryRootLease::try_clone)
                    .transpose()?,
            )
        };
        let Some(roots) = roots else { return Ok(None) };
        let version = roots.with_snapshot(|source| {
            super::Storage::probe_spill_snapshot(source, rowid, snapshot)
        })?;
        let Some(version) = version else {
            return Ok(None);
        };
        let source = SpilledRowState::new(roots, rowid, version);
        let selected = source.visible_at(snapshot)?;
        if selected
            .as_ref()
            .is_some_and(|read| read.snapshot().created_at != created_at)
        {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "retained object row belongs to a different table or version"
            ));
        }
        Ok(selected.map(super::VisibleRowHome::spilled))
    }

    pub(super) fn pin_owned(
        &self,
        table: usize,
        rowid: u64,
        selected: Option<(RowVersionIdentity, Option<RowHome>)>,
    ) -> Result<Option<super::VisibleRowHome>, SqlError> {
        let VersionOwner::Retained(lease) = &self.versions else {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "row metadata has no owned source"
            ));
        };
        let image = lease.read();
        let source = image.sources.as_ref().ok_or_else(|| {
            sql_err!(sqlstate::INTERNAL_ERROR, "row metadata has no owned source")
        })?;
        if source.table != table || source.rowid != rowid {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "retained row metadata does not match the requested table and row"
            ));
        }
        let Some((version, Some(home))) = selected else {
            return Ok(None);
        };
        let snapshot = super::RowSnapshot {
            created_at: source.created_at,
            version,
        };
        match home {
            RowHome::Heap(location) => {
                let heap = source.heap.as_ref().ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "retained row metadata has no heap source"
                    )
                })?;
                Ok(Some(super::VisibleRowHome::heap(
                    snapshot,
                    heap.read_location(location)?,
                    table,
                    rowid,
                )))
            }
            RowHome::Spilled {
                len,
                sst,
                commit_lsn,
            } => {
                let roots = source.objects.as_ref().ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "retained row metadata has no object source"
                    )
                })?;
                let read = SpilledRowRead::new(roots.try_clone()?, rowid, len, sst, commit_lsn)?;
                if read.snapshot().created_at != snapshot.created_at
                    || read.snapshot().version != snapshot.version
                {
                    return Err(sql_err!(
                        sqlstate::SERIALIZATION_FAILURE,
                        "retained object row belongs to a different table or version"
                    ));
                }
                Ok(Some(super::VisibleRowHome::spilled(read)))
            }
        }
    }
    fn versions(&self) -> Option<&RowVersionState> {
        match &self.versions {
            VersionOwner::Retained(_) => None,
            VersionOwner::Borrowed(owner) => Some(owner),
            VersionOwner::Immutable(_) => None,
        }
    }

    /// Immutable SST metadata has no resident chain handles or lock owner.
    pub(super) fn immutable(source: SpilledRowState) -> Self {
        let state = source.state();
        Self {
            state,
            versions: VersionOwner::Immutable(source),
            captured_roots: None,
        }
    }

    pub(super) fn with_captured_roots<'scan>(
        self,
        roots: &'scan CapturedSpillRoots<'scan>,
    ) -> RowRead<'scan>
    where
        'owner: 'scan,
    {
        RowRead {
            state: self.state,
            versions: self.versions,
            captured_roots: Some(roots),
        }
    }

    pub(super) fn captured_roots(&self) -> Option<&'owner CapturedSpillRoots<'owner>> {
        self.captured_roots
    }

    pub(super) fn spilled_source(&self) -> Option<&SpilledRowState> {
        match &self.versions {
            VersionOwner::Immutable(source) => Some(source),
            VersionOwner::Borrowed(_) | VersionOwner::Retained(_) => None,
        }
    }

    pub(crate) fn pending_last(&self) -> Option<PendingChange> {
        if let VersionOwner::Retained(lease) = &self.versions {
            let image = lease.read();
            return image.pending.first().map(|(_, change)| *change);
        }
        pending_last(&self.versions()?.pending_row_versions, self.state.pending)
    }

    pub(super) fn pending_head_identity(&self) -> Option<u64> {
        if let VersionOwner::Retained(lease) = &self.versions {
            let image = lease.read();
            return image.pending.first().map(|(identity, _)| *identity);
        }
        self.state.pending.tail.map(|slot| {
            self.versions()
                .expect("resident pending chain")
                .pending_row_versions[slot]
                .identity
        })
    }

    pub(crate) fn history_get(&self, index: usize) -> Option<CommittedVersion> {
        if let VersionOwner::Retained(lease) = &self.versions {
            let image = lease.read();
            return image.history.get(index).copied();
        }
        committed_history_get(
            &self.versions()?.committed_row_versions,
            self.state.history,
            index,
        )
    }

    pub(crate) fn locked_by_other(&self, txid: u32) -> Option<u32> {
        self.pending_last()
            .filter(|change| change.txid != txid)
            .map(|change| change.txid)
    }

    pub(super) fn has_visible_pending(&self, txid: u32, snapshot: u32) -> bool {
        if let VersionOwner::Retained(lease) = &self.versions {
            let image = lease.read();
            return image
                .pending
                .iter()
                .any(|(_, change)| change.txid == txid && change.cid < snapshot);
        }
        self.versions().is_some_and(|versions| {
            pending_visible_at(
                &versions.pending_row_versions,
                self.state.pending,
                txid,
                snapshot,
            )
            .is_some()
        })
    }

    pub(super) fn retained_pending_home(&self, identity: u64) -> Option<Option<RowLoc>> {
        if let VersionOwner::Retained(lease) = &self.versions {
            let image = lease.read();
            return image
                .pending
                .iter()
                .find(|(id, _)| *id == identity)
                .map(|(_, change)| change.loc);
        }
        retained_pending_home(
            &self.versions()?.pending_row_versions,
            self.state.pending,
            identity,
        )
    }

    pub(super) fn visible_version_at(
        &self,
        txid: u32,
        command_snapshot: u32,
        commit_snapshot: u64,
    ) -> Option<(RowVersionIdentity, Option<RowHome>)> {
        if let VersionOwner::Retained(lease) = &self.versions {
            let image = lease.read();
            if let Some((identity, change)) = image
                .pending
                .iter()
                .find(|(_, change)| change.txid == txid && change.cid < command_snapshot)
            {
                return Some((
                    RowVersionIdentity::Pending(*identity),
                    change.loc.map(RowHome::Heap),
                ));
            }
            if self.state.committed_lsn <= commit_snapshot {
                return Some((
                    RowVersionIdentity::Committed(self.state.committed_lsn),
                    self.state.committed,
                ));
            }
            return image
                .history
                .iter()
                .find(|version| version.lsn <= commit_snapshot)
                .map(|version| (RowVersionIdentity::Committed(version.lsn), version.home));
        }
        if let Some(versions) = self.versions()
            && let Some((identity, location)) = pending_version_at(
                &versions.pending_row_versions,
                self.state.pending,
                txid,
                command_snapshot,
            )
        {
            return Some((
                RowVersionIdentity::Pending(identity),
                location.map(RowHome::Heap),
            ));
        }
        if self.state.committed_lsn <= commit_snapshot {
            return Some((
                RowVersionIdentity::Committed(self.state.committed_lsn),
                self.state.committed,
            ));
        }
        let version = committed_version_at(
            &self.versions()?.committed_row_versions,
            self.state.history,
            commit_snapshot,
        )?;
        Some((RowVersionIdentity::Committed(version.lsn), version.home))
    }

    /// `None` requests immutable history; `Some(None)` is a visible deletion.
    pub(super) fn visible_at(
        &self,
        txid: u32,
        command_snapshot: u32,
        commit_snapshot: u64,
    ) -> Option<Option<RowHome>> {
        self.visible_version_at(txid, command_snapshot, commit_snapshot)
            .map(|(_, home)| home)
    }
}

/// Acquire version ownership before map ownership. A complete walk borrows
/// this owner rather than recursively acquiring the pool lock for each row.
pub(crate) struct RowReadView<'a> {
    rows: RwLockReadGuard<'a, RowEntries>,
    versions: RwLockReadGuard<'a, RowVersionState>,
}

impl<'a> RowReadView<'a> {
    pub(super) fn new(versions: &'a RowVersionPools, rows: &'a RowMap) -> Self {
        let (versions, rows) = super::row_ownership::acquire(|| {
            Ok((versions.try_read_for_batch()?, rows.try_read_for_batch()?))
        });
        Self { rows, versions }
    }

    #[cfg(test)]
    pub(super) fn point(
        versions: &RowVersionPools,
        rows: &RowMap,
        rowid: u64,
    ) -> Result<Option<RowRead<'static>>, SqlError> {
        let (owner, rows) = super::row_ownership::acquire(|| {
            Ok((versions.try_read_for_batch()?, rows.try_read_for_batch()?))
        });
        let state = rows.get(&rowid).copied();
        drop(rows);
        Self::capture_with(versions, &owner, state, |_, _| Ok(None))
    }

    pub(super) fn capture_with(
        versions: &RowVersionPools,
        owner: &RowVersionState,
        state: Option<RowState>,
        sources: impl FnOnce(
            RowState,
            &CapturedRowMetadata,
        ) -> Result<Option<CapturedRowSources>, SqlError>,
    ) -> Result<Option<RowRead<'static>>, SqlError> {
        let Some(state) = state else {
            return Ok(None);
        };
        let image = versions
            .metadata
            .capture(owner, state, |image| sources(state, image))?;
        Ok(Some(RowRead {
            state,
            versions: VersionOwner::Retained(image),
            captured_roots: None,
        }))
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (u64, RowRead<'_>)> {
        self.rows
            .iter()
            .map(|(&rowid, &state)| (rowid, RowRead::borrowed(state, &self.versions)))
    }
}

#[cfg(test)]
mod tests {
    use super::super::row_versions::{
        clear_pending_versions, push_committed_version, push_pending_version,
        release_committed_chain,
    };
    use super::super::{ColumnSet, CommittedHistory, PendingVersions, RowLoc};
    use super::*;
    use crate::config::Config;
    use crate::mem::budget::Budget;

    fn owners() -> (RowVersionPools, RowMap) {
        let mut config = Config::default_dev();
        config.max_connections = 1;
        config.max_prepared_transactions = 0;
        config.txn_rows = 1;
        config.max_tables = 1;
        config.table_rows = 1;
        config.large_object_pages = 1;
        config.max_row_versions_per_row = 2;
        let mut budget = Budget::new(1 << 20);
        (
            RowVersionPools::new(&config, &mut budget).unwrap(),
            RowMap::new(&mut budget, "reader rows", 1).unwrap(),
        )
    }

    fn install(versions: &mut RowVersionState, rows: &mut RowEntries, epoch: u32) {
        for (_, state) in rows.iter() {
            let mut pending = state.pending;
            clear_pending_versions(&mut versions.pending_row_versions, &mut pending);
            release_committed_chain(&mut versions.committed_row_versions, state.history.tail);
        }
        rows.clear();
        let location = RowLoc::test(epoch, 4);
        let mut state = RowState::committed_only_at(location, u64::from(epoch));
        state.pending = PendingVersions::empty();
        state.history = CommittedHistory::empty();
        push_pending_version(
            &mut versions.pending_row_versions,
            &mut state.pending,
            2,
            PendingChange {
                txid: 7,
                cid: epoch,
                loc: Some(location),
                changed_columns: ColumnSet::EMPTY,
                changes_existence: false,
            },
        )
        .unwrap();
        push_committed_version(
            &mut versions.committed_row_versions,
            &mut state.history,
            2,
            CommittedVersion {
                lsn: u64::from(epoch - 1),
                home: None,
            },
        )
        .unwrap();
        rows.insert(u64::from(epoch % 2 + 1), state).unwrap();
    }

    fn observe(row: &RowRead<'_>) {
        let pending = row.pending_last().unwrap();
        let history = row.history_get(0).unwrap();
        assert_eq!(u64::from(pending.cid), row.committed_lsn);
        assert_eq!(history.lsn + 1, row.committed_lsn);
        assert_eq!(
            row.visible_at(7, pending.cid + 1, history.lsn),
            Some(pending.loc.map(RowHome::Heap))
        );
        assert_eq!(row.visible_at(8, pending.cid + 1, history.lsn), Some(None));
        assert_eq!(row.locked_by_other(8), Some(7));
        assert_eq!(row.locked_by_other(7), None);
    }

    #[test]
    fn row_read_ownership_point_copies_survive_chain_slot_reuse() {
        let (versions, rows) = owners();
        install(
            &mut versions.test_write().unwrap(),
            &mut rows.test_write().unwrap(),
            1,
        );
        crate::mem::guard::forbid_alloc(|| {
            let row = RowReadView::point(&versions, &rows, 2).unwrap().unwrap();
            observe(&row);
            let copied = row.pending_last().unwrap();
            assert!(versions.test_write().is_ok());
            install(
                &mut versions.test_write().unwrap(),
                &mut rows.test_write().unwrap(),
                2,
            );
            observe(&row);
            assert_eq!(row.pending_last().unwrap().cid, 1);
            drop(row);
            assert!(RowReadView::point(&versions, &rows, 2).unwrap().is_none());
            let row = RowReadView::point(&versions, &rows, 1).unwrap().unwrap();
            observe(&row);
            assert_eq!(copied.cid, 1);
            assert_eq!(row.pending_last().unwrap().cid, 2);
        });
        assert!(versions.test_write().is_ok());
        assert!(rows.test_write().is_ok());
    }

    #[test]
    fn row_read_ownership_capture_capacity_releases_on_error_and_reuse() {
        let (versions, rows) = owners();
        install(
            &mut versions.test_write().unwrap(),
            &mut rows.test_write().unwrap(),
            1,
        );
        crate::mem::guard::forbid_alloc(|| {
            let mut reads: [Option<RowRead<'_>>; METADATA_READS_PER_WORKSPACE] =
                std::array::from_fn(|_| None);
            for read in &mut reads {
                *read = RowReadView::point(&versions, &rows, 2).unwrap();
            }
            let error = RowReadView::point(&versions, &rows, 2)
                .err()
                .expect("full metadata pool");
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(
                error
                    .message
                    .as_str()
                    .contains("row metadata read slots exhausted")
            );
            assert!(RowReadView::point(&versions, &rows, 999).unwrap().is_none());
            install(
                &mut versions.test_write().unwrap(),
                &mut rows.test_write().unwrap(),
                2,
            );
            for read in &reads {
                observe(read.as_ref().unwrap());
                assert_eq!(read.as_ref().unwrap().pending_last().unwrap().cid, 1);
            }
            drop(reads[0].take());
            let replacement = RowReadView::point(&versions, &rows, 1).unwrap().unwrap();
            observe(&replacement);
            assert_eq!(replacement.pending_last().unwrap().cid, 2);
            drop(replacement);
            drop(reads);
            assert!(versions.test_write().is_ok());
        });
    }

    #[test]
    fn row_read_ownership_metadata_reservation_is_exact_and_startup_bounded() {
        let mut config = Config::default_dev();
        config.query_workspace_slots = 2;
        config.max_row_versions_per_row = 3;
        let mut budget = Budget::new(RowMetadataPool::allocation_bytes(&config));
        let pool = RowMetadataPool::new(&config, &mut budget).unwrap();
        assert_eq!(budget.remaining(), 0);
        assert_eq!(pool.slots.len(), 2 * METADATA_READS_PER_WORKSPACE);
        for slot in pool.slots.iter() {
            let image = slot.image.read().unwrap();
            assert_eq!(image.pending.capacity(), 3);
            assert_eq!(image.history.capacity(), 3);
        }
    }

    #[test]
    fn owned_row_lookup_metadata_lease_outlives_its_pool_owner() {
        let (versions, rows) = owners();
        install(
            &mut versions.test_write().unwrap(),
            &mut rows.test_write().unwrap(),
            1,
        );
        crate::mem::guard::forbid_alloc(|| {
            let retained = RowReadView::point(&versions, &rows, 2).unwrap().unwrap();
            drop(rows);
            drop(versions);
            observe(&retained);
            drop(retained);
        });
    }

    #[test]
    fn row_read_ownership_walk_pins_map_and_chains_without_recursive_lookup() {
        let (versions, rows) = owners();
        install(
            &mut versions.test_write().unwrap(),
            &mut rows.test_write().unwrap(),
            1,
        );
        crate::mem::guard::forbid_alloc(|| {
            let view = RowReadView::new(&versions, &rows);
            assert!(versions.test_write().is_err());
            assert!(rows.test_write().is_err());
            for (rowid, row) in view.iter() {
                assert_eq!(rowid, 2);
                observe(&row);
            }
            drop(view);
            assert!(versions.test_write().is_ok());
            assert!(rows.test_write().is_ok());
        });
    }

    #[test]
    fn row_read_ownership_coherent_across_row_and_chain_slot_reuse() {
        let (versions, rows) = owners();
        install(
            &mut versions.test_write().unwrap(),
            &mut rows.test_write().unwrap(),
            1,
        );
        let start = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            for _ in 0..3 {
                let versions = &versions;
                let rows = &rows;
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    crate::mem::guard::forbid_alloc(|| {
                        for _ in 0..500 {
                            if let Some(row) = RowReadView::point(versions, rows, 1).unwrap() {
                                observe(&row);
                                assert_eq!(row.committed_lsn % 2, 0);
                            }
                            let view = RowReadView::new(versions, rows);
                            let mut count = 0;
                            for (rowid, row) in view.iter() {
                                observe(&row);
                                assert_eq!(rowid, row.committed_lsn % 2 + 1);
                                count += 1;
                            }
                            assert_eq!(count, 1);
                        }
                    });
                });
            }
            start.wait();
            crate::mem::guard::forbid_alloc(|| {
                for epoch in 2..=500 {
                    loop {
                        if let Ok(mut writer) = versions.test_write() {
                            let mut rows = rows.test_write().unwrap();
                            install(&mut writer, &mut rows, epoch);
                            break;
                        }
                        std::thread::yield_now();
                    }
                }
            });
        });
    }

    #[test]
    fn row_read_ownership_immutable_images_need_no_resident_owner() {
        let (versions, rows) = owners();
        let config = super::super::tests::test_config();
        let mut budget = super::super::tests::test_budget(&config);
        let mut storage = super::super::Storage::new(&config, &mut budget).unwrap();
        let table = storage
            .create_table(super::super::tests::make_def(
                "immutable_row",
                &[("id", super::super::ColType::Int4, true)],
            ))
            .unwrap();
        storage.set_spill_list(
            table,
            &[crate::store::SstHandle {
                index: crate::store::BlockId([1; 32]),
                filter: crate::store::BlockId([2; 32]),
                roster: crate::store::BlockId([3; 32]),
                format: crate::store::RowSstFormat::PackedV4,
            }],
        );
        crate::mem::guard::forbid_alloc(|| {
            assert!(RowReadView::point(&versions, &rows, 1).unwrap().is_none());
            assert!(versions.test_write().is_ok());
            let roots = storage
                .query_roots
                .retain_point(&storage, table, 9)
                .unwrap();
            let row = RowRead::immutable(SpilledRowState::new(
                roots,
                1,
                super::super::SpillVersion {
                    len: Some(4),
                    member: 0,
                    generation: crate::storage::SpillGenerationId::fixture(0),
                    commit_lsn: 9,
                },
            ));
            assert!(row.pending_last().is_none());
            assert!(row.history_get(0).is_none());
            assert_eq!(row.visible_at(7, 1, 8), None);
            assert_eq!(row.visible_at(7, 1, 9), Some(row.committed));
            assert!(versions.test_write().is_ok());
        });
    }
}
