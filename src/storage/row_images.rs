//! Visible heap images retain byte ownership; deferred reads retain snapshots.

use super::row_heap::HeapRowRead;
use super::{RowHome, RowLoc};

/// A table incarnation and the MVCC boundaries used to select a logical row.
#[derive(Clone, Copy, Debug)]
pub struct RowSnapshot {
    pub(super) created_at: u64,
    pub(super) txid: u32,
    pub(super) command: u32,
    pub(super) commit: u64,
}

/// Deferred executor reads distinguish an MVCC snapshot from unpublished bytes.
#[derive(Clone, Copy, Debug)]
pub enum RowReadSource {
    Snapshot(RowSnapshot),
    StagedHeap(RowLoc),
}

impl RowReadSource {
    pub fn heap_loc(self) -> Option<RowLoc> {
        match self {
            Self::Snapshot(_) => None,
            Self::StagedHeap(location) => Some(location),
        }
    }
}

pub struct VisibleRowHome<'a> {
    snapshot: RowSnapshot,
    pub(super) bytes: VisibleRowBytes<'a>,
}

impl core::fmt::Debug for VisibleRowHome<'_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.debug_struct("VisibleRowHome")
            .field("snapshot", &self.snapshot)
            .field("heap_location", &self.heap_loc())
            .finish_non_exhaustive()
    }
}

pub(super) enum VisibleRowBytes<'a> {
    Heap(HeapRowRead<'a>),
    Spilled { len: u32, sst: u32, commit_lsn: u64 },
}

impl<'a> VisibleRowHome<'a> {
    pub(super) fn heap(snapshot: RowSnapshot, bytes: HeapRowRead<'a>) -> Self {
        Self { snapshot, bytes: VisibleRowBytes::Heap(bytes) }
    }

    pub(super) fn spilled(snapshot: RowSnapshot, len: u32, sst: u32, commit_lsn: u64) -> Self {
        Self { snapshot, bytes: VisibleRowBytes::Spilled { len, sst, commit_lsn } }
    }

    pub fn snapshot(&self) -> RowSnapshot {
        self.snapshot
    }

    pub fn heap_loc(&self) -> Option<RowLoc> {
        match &self.bytes {
            VisibleRowBytes::Heap(bytes) => Some(bytes.location()),
            VisibleRowBytes::Spilled { .. } => None,
        }
    }

    #[cfg(test)]
    pub(super) fn metadata(&self) -> RowHome {
        match &self.bytes {
            VisibleRowBytes::Heap(bytes) => RowHome::Heap(bytes.location()),
            VisibleRowBytes::Spilled { len, sst, commit_lsn } => RowHome::Spilled {
                len: *len, sst: *sst, commit_lsn: *commit_lsn,
            },
        }
    }
}

/// The source is explicit: a pinned visible image, a frozen snapshot, or
/// physical metadata used by exclusive publication and maintenance paths.
pub struct RowByteRead<'a>(pub(super) RowByteSource<'a>);

pub(super) enum RowByteSource<'a> {
    Visible(VisibleRowHome<'a>),
    Snapshot(RowSnapshot),
    Physical(RowHome),
}

impl<'a> From<VisibleRowHome<'a>> for RowByteRead<'a> {
    fn from(image: VisibleRowHome<'a>) -> Self {
        Self(RowByteSource::Visible(image))
    }
}

impl From<RowSnapshot> for RowByteRead<'_> {
    fn from(snapshot: RowSnapshot) -> Self {
        Self(RowByteSource::Snapshot(snapshot))
    }
}

impl From<RowReadSource> for RowByteRead<'_> {
    fn from(source: RowReadSource) -> Self {
        match source {
            RowReadSource::Snapshot(snapshot) => Self(RowByteSource::Snapshot(snapshot)),
            RowReadSource::StagedHeap(location) => Self(RowByteSource::Physical(RowHome::Heap(location))),
        }
    }
}

impl From<RowHome> for RowByteRead<'_> {
    fn from(home: RowHome) -> Self {
        Self(RowByteSource::Physical(home))
    }
}
