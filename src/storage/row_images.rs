//! Visible heap images retain byte ownership; deferred reads retain snapshots.

use super::row_heap::HeapRowRead;
use super::{RowHome, RowLoc};

/// A table incarnation and the exact row version selected by MVCC.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RowSnapshot {
    pub(super) created_at: u64,
    pub(super) version: RowVersionIdentity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RowVersionIdentity {
    Pending(u64),
    Committed(u64),
}

/// One incarnation shared by all retained rows in a single-table scan.
#[derive(Clone, Copy)]
pub(crate) struct TableRowSnapshot {
    pub(super) created_at: u64,
}

/// Compact scan metadata keeps physical ordering separate from byte identity.
#[derive(Clone, Copy)]
pub(crate) struct OrderedRowSnapshot {
    rowid: u64,
    version: RowVersionIdentity,
    heap_offset: Option<u32>,
}

impl TableRowSnapshot {
    pub(crate) fn retain(
        &self,
        rowid: u64,
        image: &VisibleRowHome<'_>,
    ) -> Result<OrderedRowSnapshot, crate::sql::eval::SqlError> {
        if self.created_at != image.snapshot.created_at {
            return Err(crate::sql_err!(
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                "scan row belongs to a different table incarnation"
            ));
        }
        Ok(OrderedRowSnapshot {
            rowid,
            version: image.snapshot.version,
            heap_offset: image.heap_loc().map(|location| location.offset),
        })
    }

    pub(crate) fn row_snapshot(&self, row: OrderedRowSnapshot) -> RowSnapshot {
        RowSnapshot {
            created_at: self.created_at,
            version: row.version,
        }
    }
}

impl OrderedRowSnapshot {
    pub(crate) fn rowid(self) -> u64 {
        self.rowid
    }

    pub(crate) fn sort_key(self) -> (u8, u64, u32) {
        self.heap_offset
            .map_or((0, self.rowid, 0), |offset| (1, 0, offset))
    }
}

/// Deferred executor reads distinguish an MVCC snapshot from staged write bytes.
#[derive(Clone, Copy, Debug)]
pub(crate) enum RowReadSource {
    Snapshot(RowSnapshot),
    StagedHeap(RowLoc),
}

pub(crate) struct VisibleRowHome<'a> {
    snapshot: RowSnapshot,
    pub(super) bytes: VisibleRowBytes<'a>,
}

impl core::fmt::Debug for VisibleRowHome<'_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("VisibleRowHome")
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
        Self {
            snapshot,
            bytes: VisibleRowBytes::Heap(bytes),
        }
    }

    pub(super) fn spilled(snapshot: RowSnapshot, len: u32, sst: u32, commit_lsn: u64) -> Self {
        Self {
            snapshot,
            bytes: VisibleRowBytes::Spilled {
                len,
                sst,
                commit_lsn,
            },
        }
    }

    pub(crate) fn snapshot(&self) -> RowSnapshot {
        self.snapshot
    }

    pub(crate) fn byte_len(&self) -> u32 {
        match &self.bytes {
            VisibleRowBytes::Heap(bytes) => bytes.location().len,
            VisibleRowBytes::Spilled { len, .. } => *len,
        }
    }

    pub(crate) fn heap_loc(&self) -> Option<RowLoc> {
        match &self.bytes {
            VisibleRowBytes::Heap(bytes) => Some(bytes.location()),
            VisibleRowBytes::Spilled { .. } => None,
        }
    }

    #[cfg(test)]
    pub(super) fn metadata(&self) -> RowHome {
        match &self.bytes {
            VisibleRowBytes::Heap(bytes) => RowHome::Heap(bytes.location()),
            VisibleRowBytes::Spilled {
                len,
                sst,
                commit_lsn,
            } => RowHome::Spilled {
                len: *len,
                sst: *sst,
                commit_lsn: *commit_lsn,
            },
        }
    }
}

/// The source is explicit: a pinned visible image, a frozen snapshot, or
/// physical metadata used by exclusive publication and maintenance paths.
pub(crate) struct RowByteRead<'a>(pub(super) RowByteSource<'a>);

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
            RowReadSource::StagedHeap(location) => {
                Self(RowByteSource::Physical(RowHome::Heap(location)))
            }
        }
    }
}

impl From<RowHome> for RowByteRead<'_> {
    fn from(home: RowHome) -> Self {
        Self(RowByteSource::Physical(home))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;

    #[test]
    fn visible_row_heap_ownership_scan_metadata_preserves_budget_and_order() {
        assert!(core::mem::size_of::<Option<OrderedRowSnapshot>>() <= 32);
        let mut budget = Budget::new(1024);
        let mut heap = super::super::RowHeap::new(&mut budget, 32).unwrap();
        let (location, bytes) = heap.append(4).unwrap();
        bytes.copy_from_slice(b"row!");
        let table = TableRowSnapshot { created_at: 7 };
        let other = TableRowSnapshot { created_at: 8 };
        let image = VisibleRowHome::heap(
            RowSnapshot {
                created_at: 7,
                version: RowVersionIdentity::Pending(1),
            },
            heap.get(location).unwrap(),
        );
        let spilled = VisibleRowHome::spilled(
            RowSnapshot {
                created_at: 7,
                version: RowVersionIdentity::Committed(4),
            },
            4,
            0,
            4,
        );
        crate::mem::guard::forbid_alloc(|| {
            let resident = table.retain(1, &image).unwrap();
            let object = table.retain(u64::MAX, &spilled).unwrap();
            assert!(object.sort_key() < resident.sort_key());
            assert_eq!(object.rowid(), u64::MAX);
            assert_eq!(
                table.row_snapshot(resident).version,
                RowVersionIdentity::Pending(1)
            );
            assert_eq!(
                other.retain(1, &image).err().unwrap().sqlstate,
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE
            );
        });
    }
}
