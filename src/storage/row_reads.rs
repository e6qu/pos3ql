//! Row images whose chain handles retain the version owner that issued them.

use core::ops::Deref;
use std::sync::RwLockReadGuard;

use super::row_map::RowMap;
use super::row_versions::{
    RowVersionPools, RowVersionState, committed_history_get, committed_visible_at, pending_last,
    pending_visible_at,
};
use super::{CommittedVersion, PendingChange, RowHome, RowState};
use crate::mem::fixed_map::FixedMap;

/// Metadata is copied, but reusable chain slots remain pinned until this read
/// ends. Only the owning view can construct a read from resident metadata.
pub struct RowRead<'a> {
    state: RowState,
    versions: VersionOwner<'a>,
}

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
            pending_visible_at(&versions.pending_row_versions, self.state.pending, txid, snapshot)
                .is_some()
        })
    }

    /// `None` requests immutable history; `Some(None)` is a visible deletion.
    pub(super) fn visible_at(
        &self,
        txid: u32,
        command_snapshot: u32,
        commit_snapshot: u64,
    ) -> Option<Option<RowHome>> {
        if let Some(versions) = self.versions()
            && let Some(location) = pending_visible_at(
                &versions.pending_row_versions,
                self.state.pending,
                txid,
                command_snapshot,
            )
        {
            return Some(location.map(RowHome::Heap));
        }
        if self.state.committed_lsn <= commit_snapshot {
            return Some(self.state.committed);
        }
        self.versions().and_then(|versions| {
            committed_visible_at(
                &versions.committed_row_versions,
                self.state.history,
                commit_snapshot,
            )
        })
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
