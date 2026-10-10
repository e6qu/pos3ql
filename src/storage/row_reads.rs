//! Row images whose chain handles retain the version owner that issued them.

use core::ops::Deref;
use std::sync::RwLockReadGuard;

use super::row_images::RowVersionIdentity;
use super::row_map::RowMap;
use super::row_versions::{
    RowVersionPools, RowVersionState, committed_history_get, committed_version_at, pending_last,
    pending_version_at, pending_visible_at, retained_pending_home,
};
use super::{CommittedVersion, PendingChange, RowHome, RowLoc, RowState};
use crate::mem::fixed_map::FixedMap;

/// Metadata is copied, but reusable chain slots remain pinned until this read
/// ends. Only the owning view can construct a read from resident metadata.
pub(crate) struct RowRead<'a> {
    state: RowState,
    versions: VersionOwner<'a>,
}

pub(super) type RowReadVisitor<'a> = dyn for<'row> FnMut(
        u64,
        RowRead<'row>,
    ) -> Result<core::ops::ControlFlow<()>, crate::sql::eval::SqlError>
    + 'a;

enum VersionOwner<'a> {
    Retained(RwLockReadGuard<'a, RowVersionState>),
    Borrowed(&'a RowVersionState),
    Immutable,
}

impl Deref for RowRead<'_> {
    type Target = RowState;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl RowRead<'_> {
    fn versions(&self) -> Option<&RowVersionState> {
        match &self.versions {
            VersionOwner::Retained(owner) => Some(owner),
            VersionOwner::Borrowed(owner) => Some(owner),
            VersionOwner::Immutable => None,
        }
    }

    /// Immutable SST metadata has no resident chain handles or lock owner.
    pub(super) fn immutable(state: RowState) -> Self {
        assert!(state.pending.is_none() && state.history.is_empty());
        Self {
            state,
            versions: VersionOwner::Immutable,
        }
    }

    pub(crate) fn pending_last(&self) -> Option<PendingChange> {
        pending_last(&self.versions()?.pending_row_versions, self.state.pending)
    }

    pub(super) fn pending_head_identity(&self) -> Option<u64> {
        self.state.pending.tail.map(|slot| {
            self.versions()
                .expect("resident pending chain")
                .pending_row_versions[slot]
                .identity
        })
    }

    pub(crate) fn history_get(&self, index: usize) -> Option<CommittedVersion> {
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
    rows: RwLockReadGuard<'a, FixedMap<u64, RowState>>,
    versions: RwLockReadGuard<'a, RowVersionState>,
}

impl<'a> RowReadView<'a> {
    pub(super) fn new(versions: &'a RowVersionPools, rows: &'a RowMap) -> Self {
        let versions = versions.read();
        let rows = rows.read();
        Self { rows, versions }
    }

    pub(super) fn point(
        versions: &'a RowVersionPools,
        rows: &RowMap,
        rowid: u64,
    ) -> Option<RowRead<'a>> {
        let versions = versions.read();
        let state = rows.read().get(&rowid).copied()?;
        Some(RowRead {
            state,
            versions: VersionOwner::Retained(versions),
        })
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (u64, RowRead<'_>)> {
        self.rows.iter().map(|(&rowid, &state)| {
            (
                rowid,
                RowRead {
                    state,
                    versions: VersionOwner::Borrowed(&self.versions),
                },
            )
        })
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

    fn install(versions: &mut RowVersionState, rows: &mut FixedMap<u64, RowState>, epoch: u32) {
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
    fn row_read_ownership_point_pins_reused_slots_until_release() {
        let (versions, rows) = owners();
        install(
            &mut versions.test_write().unwrap(),
            &mut rows.test_write().unwrap(),
            1,
        );
        crate::mem::guard::forbid_alloc(|| {
            let row = RowReadView::point(&versions, &rows, 2).unwrap();
            observe(&row);
            let copied = row.pending_last().unwrap();
            assert!(versions.test_write().is_err());
            drop(row);
            install(
                &mut versions.test_write().unwrap(),
                &mut rows.test_write().unwrap(),
                2,
            );
            assert!(RowReadView::point(&versions, &rows, 2).is_none());
            let row = RowReadView::point(&versions, &rows, 1).unwrap();
            observe(&row);
            assert_eq!(copied.cid, 1);
            assert_eq!(row.pending_last().unwrap().cid, 2);
        });
        assert!(versions.test_write().is_ok());
        assert!(rows.test_write().is_ok());
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
                            if let Some(row) = RowReadView::point(versions, rows, 1) {
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
        crate::mem::guard::forbid_alloc(|| {
            assert!(RowReadView::point(&versions, &rows, 1).is_none());
            assert!(versions.test_write().is_ok());
            let row = RowRead::immutable(RowState::committed_only_at(RowLoc::test(8, 4), 9));
            assert!(row.pending_last().is_none());
            assert!(row.history_get(0).is_none());
            assert_eq!(row.visible_at(7, 1, 8), None);
            assert_eq!(row.visible_at(7, 1, 9), Some(row.committed));
            assert!(versions.test_write().is_ok());
        });
    }
}
