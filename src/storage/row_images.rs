//! Visible heap images retain byte ownership; deferred reads retain snapshots.

use super::row_heap::HeapRowRead;
use super::spill_generations::{
    CapturedSpillRoots, QueryRootPool, SpillGenerationImage, SpillGenerationList, SpilledRowRead,
};
use super::{RowHome, RowLoc};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use core::marker::PhantomData;
use core::num::NonZeroUsize;
use core::ptr::NonNull;
use std::sync::Arc;

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

/// One byte owner shared by every deferred row in a sorted scan.
pub(crate) struct TableRowSnapshot<'arena> {
    pub(super) created_at: u64,
    table_slot: usize,
    heap: Option<HeapRowRead>,
    roots: CapturedSpillRoots<'arena>,
    pool: Arc<QueryRootPool>,
}

/// Compact scan metadata keeps physical ordering separate from byte identity.
#[derive(Clone, Copy)]
pub(crate) struct OrderedRowSnapshot<'source, 'arena> {
    source: OrderedRowSource<'source, 'arena>,
    rowid: u64,
    version: u64,
    offset_or_member: u32,
    len: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
enum OrderedVersionKind {
    PendingHeap,
    CommittedHeap,
    CommittedSpilled,
}

/// The source's alignment leaves two address bits for the version kind.
/// Keep the complete identity and location widths in the 32-byte row record.
#[derive(Clone, Copy)]
struct OrderedRowSource<'source, 'arena> {
    tagged: NonNull<TableRowSnapshot<'arena>>,
    borrowed: PhantomData<&'source TableRowSnapshot<'arena>>,
}

const _: () = assert!(core::mem::align_of::<TableRowSnapshot<'static>>() >= 4);

impl<'source, 'arena> OrderedRowSource<'source, 'arena> {
    fn new(source: &'source TableRowSnapshot<'arena>, kind: OrderedVersionKind) -> Self {
        let tagged = NonNull::from(source).map_addr(|address| {
            NonZeroUsize::new(address.get() | kind as usize).expect("non-null source")
        });
        Self {
            tagged,
            borrowed: PhantomData,
        }
    }

    fn kind(self) -> OrderedVersionKind {
        match self.tagged.addr().get() & 3 {
            0 => OrderedVersionKind::PendingHeap,
            1 => OrderedVersionKind::CommittedHeap,
            2 => OrderedVersionKind::CommittedSpilled,
            _ => unreachable!("only checked version kinds are encoded"),
        }
    }

    fn owner(self) -> &'source TableRowSnapshot<'arena> {
        let pointer = self
            .tagged
            .map_addr(|address| NonZeroUsize::new(address.get() & !3).expect("non-null source"));
        // SAFETY: construction borrows this aligned source for 'source. Both
        // map_addr calls preserve provenance; clearing the two kind bits
        // restores its original address. Access is immutable, never as_mut.
        unsafe { pointer.as_ref() }
    }
}

// SAFETY: the marker retains an immutable source borrow. Copying the tagged
// pointer cannot mutate its owner; decoding yields only that shared reference.
unsafe impl<'source, 'arena> Send for OrderedRowSource<'source, 'arena> where
    TableRowSnapshot<'arena>: Sync
{
}
// SAFETY: identical to Send; shared access requires a Sync source owner.
unsafe impl<'source, 'arena> Sync for OrderedRowSource<'source, 'arena> where
    TableRowSnapshot<'arena>: Sync
{
}

impl<'arena> TableRowSnapshot<'arena> {
    pub(super) fn capture(
        storage: &super::Storage,
        image: SpillGenerationImage,
        generations: &SpillGenerationList,
        retain_heap: bool,
        arena: Option<&'arena crate::mem::arena::Arena>,
    ) -> Result<Self, SqlError> {
        Ok(Self {
            created_at: image.created_at,
            table_slot: image.table_slot(),
            heap: retain_heap.then(|| storage.heap.retain_allocation()),
            roots: CapturedSpillRoots::capture(storage, image, generations, arena)?,
            pool: Arc::clone(&storage.query_roots),
        })
    }

    pub(crate) fn retain<'source>(
        &'source self,
        rowid: u64,
        image: &VisibleRowHome,
    ) -> Result<OrderedRowSnapshot<'source, 'arena>, SqlError> {
        if self.created_at != image.snapshot.created_at {
            return Err(crate::sql_err!(
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                "scan row belongs to a different table incarnation"
            ));
        }
        image.validate_request(self.table_slot, rowid)?;
        let (version, kind, offset_or_member, len) = match &image.bytes {
            VisibleRowBytes::Heap(bytes) => {
                let heap = self
                    .heap
                    .as_ref()
                    .filter(|heap| heap.same_allocation(bytes))
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::SERIALIZATION_FAILURE,
                            "ordered row heap allocation changed"
                        )
                    })?;
                let location = bytes.location();
                heap.other(location)?;
                let (version, kind) = match image.snapshot.version {
                    RowVersionIdentity::Pending(identity) => {
                        (identity, OrderedVersionKind::PendingHeap)
                    }
                    RowVersionIdentity::Committed(lsn) => (lsn, OrderedVersionKind::CommittedHeap),
                };
                (version, kind, location.offset, location.len)
            }
            VisibleRowBytes::Spilled(read) => {
                if !self.roots.matches_read(read)? {
                    return Err(sql_err!(
                        sqlstate::SERIALIZATION_FAILURE,
                        "ordered row saved-file source changed"
                    ));
                }
                (
                    read.commit_lsn,
                    OrderedVersionKind::CommittedSpilled,
                    read.member,
                    read.len,
                )
            }
        };
        Ok(OrderedRowSnapshot {
            source: OrderedRowSource::new(self, kind),
            rowid,
            version,
            offset_or_member,
            len,
        })
    }
}

impl OrderedRowSnapshot<'_, '_> {
    pub(crate) fn rowid(self) -> u64 {
        self.rowid
    }

    pub(crate) fn sort_key(self) -> (u8, u64, u32) {
        match self.source.kind() {
            OrderedVersionKind::CommittedSpilled => (0, self.rowid, 0),
            _ => (1, 0, self.offset_or_member),
        }
    }

    pub(crate) fn read(self) -> Result<VisibleRowHome, SqlError> {
        let source = self.source.owner();
        match self.source.kind() {
            OrderedVersionKind::PendingHeap | OrderedVersionKind::CommittedHeap => {
                let heap = source
                    .heap
                    .as_ref()
                    .expect("retained heap row has an owner");
                let version = match self.source.kind() {
                    OrderedVersionKind::PendingHeap => RowVersionIdentity::Pending(self.version),
                    _ => RowVersionIdentity::Committed(self.version),
                };
                let location = RowLoc {
                    offset: self.offset_or_member,
                    len: self.len,
                    generation: heap.location().generation,
                };
                Ok(VisibleRowHome::heap(
                    RowSnapshot {
                        created_at: source.created_at,
                        version,
                    },
                    heap.read_location(location)?,
                    source.table_slot,
                    self.rowid,
                ))
            }
            OrderedVersionKind::CommittedSpilled => {
                let roots = source
                    .roots
                    .owned_source_in(&source.pool)?
                    .expect("retained object row has an owner");
                SpilledRowRead::new(
                    roots,
                    self.rowid,
                    self.len,
                    self.offset_or_member,
                    self.version,
                )
                .map(VisibleRowHome::spilled)
            }
        }
    }
}

/// Deferred executor reads distinguish an MVCC snapshot from staged write bytes.
#[derive(Clone, Copy, Debug)]
pub(crate) enum RowReadSource {
    Snapshot(RowSnapshot),
    StagedHeap(RowLoc),
}

pub(crate) struct VisibleRowHome {
    snapshot: RowSnapshot,
    table_slot: usize,
    rowid: u64,
    pub(super) bytes: VisibleRowBytes,
}

impl core::fmt::Debug for VisibleRowHome {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("VisibleRowHome")
            .field("snapshot", &self.snapshot)
            .field("heap_location", &self.heap_loc())
            .finish_non_exhaustive()
    }
}

pub(super) enum VisibleRowBytes {
    Heap(HeapRowRead),
    Spilled(SpilledRowRead),
}

impl VisibleRowHome {
    pub(super) fn heap(
        snapshot: RowSnapshot,
        bytes: HeapRowRead,
        table_slot: usize,
        rowid: u64,
    ) -> Self {
        Self {
            snapshot,
            table_slot,
            rowid,
            bytes: VisibleRowBytes::Heap(bytes),
        }
    }

    pub(super) fn spilled(source: SpilledRowRead) -> Self {
        Self {
            snapshot: source.snapshot(),
            table_slot: source.table_slot,
            rowid: source.rowid,
            bytes: VisibleRowBytes::Spilled(source),
        }
    }

    pub(super) fn validate_request(
        &self,
        table: usize,
        rowid: u64,
    ) -> Result<(), crate::sql::eval::SqlError> {
        if table != self.table_slot || rowid != self.rowid {
            return Err(crate::sql_err!(
                crate::sql::eval::sqlstate::INTERNAL_ERROR,
                "retained row does not match the requested table and row"
            ));
        }
        Ok(())
    }

    pub(crate) fn snapshot(&self) -> RowSnapshot {
        self.snapshot
    }

    pub(crate) fn byte_len(&self) -> u32 {
        match &self.bytes {
            VisibleRowBytes::Heap(bytes) => bytes.location().len,
            VisibleRowBytes::Spilled(source) => source.len,
        }
    }

    pub(crate) fn heap_loc(&self) -> Option<RowLoc> {
        match &self.bytes {
            VisibleRowBytes::Heap(bytes) => Some(bytes.location()),
            VisibleRowBytes::Spilled(_) => None,
        }
    }

    #[cfg(test)]
    pub(super) fn metadata(&self) -> RowHome {
        match &self.bytes {
            VisibleRowBytes::Heap(bytes) => RowHome::Heap(bytes.location()),
            VisibleRowBytes::Spilled(source) => RowHome::Spilled {
                len: source.len,
                sst: source.member,
                commit_lsn: source.commit_lsn,
            },
        }
    }
}

/// The source is explicit: a pinned visible image, a frozen snapshot, or
/// physical metadata used by exclusive publication and maintenance paths.
pub(crate) struct RowByteRead(pub(super) RowByteSource);

pub(super) enum RowByteSource {
    Visible(VisibleRowHome),
    Snapshot(RowSnapshot),
    Physical(RowHome),
}

impl From<VisibleRowHome> for RowByteRead {
    fn from(image: VisibleRowHome) -> Self {
        Self(RowByteSource::Visible(image))
    }
}

impl From<RowSnapshot> for RowByteRead {
    fn from(snapshot: RowSnapshot) -> Self {
        Self(RowByteSource::Snapshot(snapshot))
    }
}

impl From<RowReadSource> for RowByteRead {
    fn from(source: RowReadSource) -> Self {
        match source {
            RowReadSource::Snapshot(snapshot) => Self(RowByteSource::Snapshot(snapshot)),
            RowReadSource::StagedHeap(location) => {
                Self(RowByteSource::Physical(RowHome::Heap(location)))
            }
        }
    }
}

impl From<RowHome> for RowByteRead {
    fn from(home: RowHome) -> Self {
        Self(RowByteSource::Physical(home))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;

    #[test]
    fn owned_ordered_source_tags_preserve_pointer_provenance() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<OrderedRowSnapshot<'static, 'static>>();
        let mut config = super::super::tests::test_config();
        // This heap-only fixture never requests an object source slot.
        config.query_workspace_slots = 0;
        let mut budget = Budget::new(QueryRootPool::budget_bytes(&config));
        let pool = QueryRootPool::new(&config, &mut budget).unwrap();
        let source = TableRowSnapshot {
            created_at: u64::MAX,
            table_slot: 17,
            heap: None,
            roots: CapturedSpillRoots::Empty,
            pool,
        };
        crate::mem::guard::forbid_alloc(|| {
            for kind in [
                OrderedVersionKind::PendingHeap,
                OrderedVersionKind::CommittedHeap,
                OrderedVersionKind::CommittedSpilled,
            ] {
                let tagged = OrderedRowSource::new(&source, kind);
                let copied = tagged;
                assert_eq!(copied.kind(), kind);
                assert!(core::ptr::eq(copied.owner(), &source));
                assert_eq!(copied.owner().created_at, u64::MAX);
                assert_eq!(copied.owner().table_slot, 17);
            }
            assert_eq!(
                core::mem::size_of::<Option<OrderedRowSnapshot<'_, '_>>>(),
                32
            );
        });
    }

    #[test]
    fn owned_ordered_rows_keep_heap_versions_after_retirement_and_relocation() {
        use super::super::tests::{make_def, test_budget, test_config};
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = super::super::Storage::new(&config, &mut budget).unwrap();
        let table = storage
            .create_table(make_def(
                "ordered_old",
                &[("id", super::super::ColType::Int4, true)],
            ))
            .unwrap();
        let mut scratch =
            crate::mem::fixed_vec::FixedVec::new(&mut budget, "ordered relocation", 8).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let locations = [
                storage.heap.append_bytes(b"hist").unwrap(),
                storage.heap.append_bytes(b"keep").unwrap(),
                storage.heap.append_bytes(b"pend").unwrap(),
            ];
            storage.tables[table]
                .rows
                .insert(
                    1,
                    super::super::RowState::committed_only_at(locations[1], 7),
                )
                .unwrap();
            let source = storage.table_row_snapshot(table, None).unwrap();
            let versions = [
                RowVersionIdentity::Committed(6),
                RowVersionIdentity::Committed(7),
                RowVersionIdentity::Pending(19),
            ];
            let records = core::array::from_fn::<_, 3, _>(|index| {
                let image = VisibleRowHome::heap(
                    RowSnapshot {
                        created_at: source.created_at,
                        version: versions[index],
                    },
                    storage.heap.get(locations[index]).unwrap(),
                    table,
                    1,
                );
                source.retain(1, &image).unwrap()
            });
            assert!(records[0].sort_key() < records[1].sort_key());
            assert!(records[1].sort_key() < records[2].sort_key());
            storage.commit_drop(table);
            let replacement = storage
                .create_table(make_def(
                    "ordered_new",
                    &[("id", super::super::ColType::Text, true)],
                ))
                .unwrap();
            assert_eq!(replacement, table);
            let location = storage.heap.append_bytes(b"next").unwrap();
            storage.tables[table]
                .rows
                .insert(1, super::super::RowState::committed_only_at(location, 8))
                .unwrap();
            storage.compact_heap(&mut scratch).unwrap();
            drop(storage);
            for (index, expected) in [b"hist", b"keep", b"pend"].into_iter().enumerate() {
                let image = records[index].read().unwrap();
                assert_eq!(image.snapshot().version, versions[index]);
                image.validate_request(table, 1).unwrap();
                let VisibleRowBytes::Heap(bytes) = image.bytes else {
                    panic!("heap fixture")
                };
                assert_eq!(&*bytes, expected);
            }
        });
    }

    #[test]
    fn owned_ordered_rows_reject_changed_heap_owners_and_wrong_requests() {
        use super::super::tests::{make_def, test_budget, test_config};
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = super::super::Storage::new(&config, &mut budget).unwrap();
        let table = storage
            .create_table(make_def(
                "ordered_owner",
                &[("id", super::super::ColType::Int4, true)],
            ))
            .unwrap();
        let location = storage.heap.append_bytes(b"keep").unwrap();
        storage.tables[table]
            .rows
            .insert(1, super::super::RowState::committed_only_at(location, 7))
            .unwrap();
        let mut scratch =
            crate::mem::fixed_vec::FixedVec::new(&mut budget, "ordered owner relocation", 8)
                .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let source = storage.table_row_snapshot(table, None).unwrap();
            let retained = VisibleRowHome::heap(
                RowSnapshot {
                    created_at: source.created_at,
                    version: RowVersionIdentity::Committed(7),
                },
                storage.heap.get(location).unwrap(),
                table,
                1,
            );
            assert_eq!(
                source.retain(2, &retained).err().unwrap().sqlstate,
                sqlstate::INTERNAL_ERROR
            );
            storage.compact_heap(&mut scratch).unwrap();
            let current = storage.tables[table]
                .rows
                .get(&1)
                .unwrap()
                .committed
                .unwrap();
            let RowHome::Heap(location) = current else {
                panic!("heap fixture")
            };
            let moved = VisibleRowHome::heap(
                RowSnapshot {
                    created_at: source.created_at,
                    version: RowVersionIdentity::Committed(7),
                },
                storage.heap.get(location).unwrap(),
                table,
                1,
            );
            assert_eq!(
                source.retain(1, &moved).err().unwrap().sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            let record = source.retain(1, &retained).unwrap();
            assert_eq!(
                record.read().unwrap().snapshot().version,
                RowVersionIdentity::Committed(7)
            );
        });
    }

    #[test]
    fn visible_row_heap_ownership_scan_metadata_preserves_budget_and_order() {
        assert_eq!(
            core::mem::size_of::<Option<OrderedRowSnapshot<'_, '_>>>(),
            32
        );
        let mut budget = Budget::new(1024);
        let arena = crate::mem::arena::Arena::new(&mut budget, "ordered source", 1024).unwrap();
        let config = super::super::tests::test_config();
        let mut object_budget = super::super::tests::test_budget(&config);
        let mut storage = super::super::Storage::new(&config, &mut object_budget).unwrap();
        let slot = storage
            .create_table(super::super::tests::make_def(
                "ordered_row",
                &[("id", super::super::ColType::Int4, true)],
            ))
            .unwrap();
        let (location, bytes) = storage.heap.append(4).unwrap();
        bytes.copy_from_slice(b"row!");
        storage.tables[slot]
            .rows
            .insert(1, super::super::RowState::committed_only_at(location, 4))
            .unwrap();
        storage.set_spill_list(
            slot,
            &[crate::store::SstHandle {
                index: crate::store::BlockId([1; 32]),
                filter: crate::store::BlockId([2; 32]),
                roster: crate::store::BlockId([3; 32]),
                format: crate::store::RowSstFormat::PackedV4,
            }],
        );
        let created_at = storage.table(slot).created_at();
        let table = storage.table_row_snapshot(slot, Some(&arena)).unwrap();
        let mut other = storage.table_row_snapshot(slot, Some(&arena)).unwrap();
        other.created_at += 1;
        let image = VisibleRowHome::heap(
            RowSnapshot {
                created_at,
                version: RowVersionIdentity::Pending(1),
            },
            storage.heap.get(location).unwrap(),
            slot,
            1,
        );
        let spilled =
            VisibleRowHome::spilled(storage.retain_spilled_row(slot, u64::MAX, 4, 0, 4).unwrap());
        crate::mem::guard::forbid_alloc(|| {
            let resident = table.retain(1, &image).unwrap();
            let object = table.retain(u64::MAX, &spilled).unwrap();
            assert!(object.sort_key() < resident.sort_key());
            assert_eq!(object.rowid(), u64::MAX);
            assert_eq!(
                resident.read().unwrap().snapshot().version,
                RowVersionIdentity::Pending(1)
            );
            assert_eq!(
                other.retain(1, &image).err().unwrap().sqlstate,
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE
            );
        });
    }
}
