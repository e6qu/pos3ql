//! Visible heap images retain byte ownership; deferred reads retain snapshots.

use super::row_heap::{HeapReadSlot, HeapReadView, HeapRowRead};
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

impl RowSnapshot {
    pub(crate) fn committed_lsn(self) -> Option<u64> {
        match self.version {
            RowVersionIdentity::Committed(lsn) => Some(lsn),
            RowVersionIdentity::Pending(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RowVersionIdentity {
    Pending(u64),
    Committed(u64),
}

/// Byte owners shared by every deferred row in a scan or table rewrite.
#[repr(align(8))]
pub(crate) struct TableRowSnapshot<'arena> {
    pub(super) created_at: u64,
    table_slot: usize,
    heap: Option<HeapReadView>,
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

/// Private scan records remain valid when their containing source is moved.
#[derive(Clone, Copy)]
pub(super) struct CapturedRowRecord {
    pub(super) rowid: u64,
    version: u64,
    offset_or_member: u32,
    len: u32,
    kind: CapturedRowKind,
}

#[derive(Clone, Copy)]
enum CapturedRowKind {
    PendingPrimaryHeap,
    CommittedPrimaryHeap,
    CommittedSpilled,
    Invisible,
    LookupSaved,
    PendingSecondaryHeap,
    CommittedSecondaryHeap,
}

impl CapturedRowRecord {
    pub(super) const EMPTY: Self = Self {
        rowid: 0,
        version: 0,
        offset_or_member: 0,
        len: 0,
        kind: CapturedRowKind::Invisible,
    };
    fn empty(rowid: u64, kind: CapturedRowKind) -> Self {
        Self {
            rowid,
            version: 0,
            offset_or_member: 0,
            len: 0,
            kind,
        }
    }

    pub(super) fn needs_saved_lookup(&self) -> bool {
        matches!(self.kind, CapturedRowKind::LookupSaved)
    }
}

const _: () = assert!(size_of::<CapturedRowRecord>() == 32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
enum OrderedVersionKind {
    PendingPrimaryHeap,
    CommittedPrimaryHeap,
    CommittedSpilled,
    PendingSecondaryHeap,
    CommittedSecondaryHeap,
}

/// The source's alignment leaves three address bits for the version kind.
/// Keep the complete identity and location widths in the 32-byte row record.
#[derive(Clone, Copy)]
struct OrderedRowSource<'source, 'arena> {
    tagged: NonNull<TableRowSnapshot<'arena>>,
    borrowed: PhantomData<&'source TableRowSnapshot<'arena>>,
}

const _: () = assert!(core::mem::align_of::<TableRowSnapshot<'static>>() >= 8);

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
        match self.tagged.addr().get() & 7 {
            0 => OrderedVersionKind::PendingPrimaryHeap,
            1 => OrderedVersionKind::CommittedPrimaryHeap,
            2 => OrderedVersionKind::CommittedSpilled,
            3 => OrderedVersionKind::PendingSecondaryHeap,
            4 => OrderedVersionKind::CommittedSecondaryHeap,
            _ => unreachable!("only checked version kinds are encoded"),
        }
    }

    fn owner(self) -> &'source TableRowSnapshot<'arena> {
        let pointer = self
            .tagged
            .map_addr(|address| NonZeroUsize::new(address.get() & !7).expect("non-null source"));
        // SAFETY: construction borrows this aligned source for 'source. Both
        // map_addr calls preserve provenance; clearing the three kind bits
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
    pub(crate) fn matches_table(&self, slot: usize, created_at: u64) -> bool {
        self.table_slot == slot && self.created_at == created_at
    }

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
            heap: retain_heap.then(|| storage.heap.retain_read_view()),
            roots: CapturedSpillRoots::capture(storage, image, generations, arena)?,
            pool: Arc::clone(&storage.query_roots),
        })
    }

    /// Reads a scheduled committed version from this capture's files, even
    /// after the live heap or table slot has been replaced.
    pub(crate) fn with_physical_row_bytes<R>(
        &self,
        rowid: u64,
        home: RowHome,
        read: impl FnOnce(&[u8]) -> Result<R, SqlError>,
    ) -> Result<R, SqlError> {
        match home {
            RowHome::Heap(location) => {
                let heap = self.heap.as_ref().ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "checkpoint capture has no heap source"
                    )
                })?;
                read(heap.other(location)?)
            }
            RowHome::Spilled {
                len,
                sst,
                commit_lsn,
            } => {
                let roots = self.roots.owned_source_in(&self.pool)?.ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "checkpoint capture has no saved-file source"
                    )
                })?;
                SpilledRowRead::new(roots, rowid, len, sst, commit_lsn)?.with_bytes(read)
            }
        }
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
                let heap = self.heap.as_ref().ok_or_else(|| {
                    sql_err!(
                        sqlstate::SERIALIZATION_FAILURE,
                        "ordered row heap allocation changed"
                    )
                })?;
                let slot = heap.slot_for(bytes)?;
                let location = bytes.location();
                heap.other(location)?;
                let (version, kind) = match (image.snapshot.version, slot) {
                    (RowVersionIdentity::Pending(identity), HeapReadSlot::Primary) => {
                        (identity, OrderedVersionKind::PendingPrimaryHeap)
                    }
                    (RowVersionIdentity::Pending(identity), HeapReadSlot::Secondary) => {
                        (identity, OrderedVersionKind::PendingSecondaryHeap)
                    }
                    (RowVersionIdentity::Committed(lsn), HeapReadSlot::Primary) => {
                        (lsn, OrderedVersionKind::CommittedPrimaryHeap)
                    }
                    (RowVersionIdentity::Committed(lsn), HeapReadSlot::Secondary) => {
                        (lsn, OrderedVersionKind::CommittedSecondaryHeap)
                    }
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
                    read.member.encoded(),
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

    pub(super) fn capture_selection(
        &self,
        rowid: u64,
        selected: Option<(RowVersionIdentity, Option<RowHome>)>,
    ) -> Result<CapturedRowRecord, SqlError> {
        let Some((version, home)) = selected else {
            return Ok(CapturedRowRecord::empty(
                rowid,
                CapturedRowKind::LookupSaved,
            ));
        };
        let Some(home) = home else {
            return Ok(CapturedRowRecord::empty(rowid, CapturedRowKind::Invisible));
        };
        let visible = match home {
            RowHome::Heap(location) => VisibleRowHome::heap(
                RowSnapshot {
                    created_at: self.created_at,
                    version,
                },
                self.heap
                    .as_ref()
                    .ok_or_else(|| {
                        sql_err!(sqlstate::INTERNAL_ERROR, "captured scan has no heap source")
                    })?
                    .read_location(location)?,
                self.table_slot,
                rowid,
            ),
            RowHome::Spilled {
                len,
                sst,
                commit_lsn,
            } => {
                if version != RowVersionIdentity::Committed(commit_lsn) {
                    return Err(sql_err!(
                        sqlstate::SERIALIZATION_FAILURE,
                        "captured scan saved-file version changed"
                    ));
                }
                VisibleRowHome::spilled(SpilledRowRead::new(
                    self.roots.owned_source_in(&self.pool)?.ok_or_else(|| {
                        sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "captured scan has no saved-file source"
                        )
                    })?,
                    rowid,
                    len,
                    sst,
                    commit_lsn,
                )?)
            }
        };
        self.capture_image(rowid, &visible)
    }

    fn capture_image(
        &self,
        rowid: u64,
        visible: &VisibleRowHome,
    ) -> Result<CapturedRowRecord, SqlError> {
        let row = self.retain(rowid, visible)?;
        Ok(CapturedRowRecord {
            rowid,
            version: row.version,
            offset_or_member: row.offset_or_member,
            len: row.len,
            kind: match row.source.kind() {
                OrderedVersionKind::PendingPrimaryHeap => CapturedRowKind::PendingPrimaryHeap,
                OrderedVersionKind::CommittedPrimaryHeap => CapturedRowKind::CommittedPrimaryHeap,
                OrderedVersionKind::CommittedSpilled => CapturedRowKind::CommittedSpilled,
                OrderedVersionKind::PendingSecondaryHeap => CapturedRowKind::PendingSecondaryHeap,
                OrderedVersionKind::CommittedSecondaryHeap => {
                    CapturedRowKind::CommittedSecondaryHeap
                }
            },
        })
    }

    pub(super) fn resolve_saved(
        &self,
        row: &mut CapturedRowRecord,
        snapshot: u64,
    ) -> Result<(), SqlError> {
        if !row.needs_saved_lookup() {
            return Ok(());
        }
        let Some(roots) = self.roots.owned_source_in(&self.pool)? else {
            row.kind = CapturedRowKind::Invisible;
            return Ok(());
        };
        let selected = roots.with_snapshot(|source| {
            super::Storage::probe_spill_snapshot(source, row.rowid, snapshot)
        })?;
        let Some(selected) = selected else {
            row.kind = CapturedRowKind::Invisible;
            return Ok(());
        };
        let visible = super::spill_generations::SpilledRowState::new(roots, row.rowid, selected)
            .visible_at(snapshot)?;
        *row = match visible {
            Some(visible) => self.capture_image(row.rowid, &VisibleRowHome::spilled(visible))?,
            None => CapturedRowRecord::empty(row.rowid, CapturedRowKind::Invisible),
        };
        Ok(())
    }

    pub(super) fn read_record(
        &self,
        row: CapturedRowRecord,
    ) -> Result<Option<VisibleRowHome>, SqlError> {
        let kind = match row.kind {
            CapturedRowKind::PendingPrimaryHeap => OrderedVersionKind::PendingPrimaryHeap,
            CapturedRowKind::CommittedPrimaryHeap => OrderedVersionKind::CommittedPrimaryHeap,
            CapturedRowKind::CommittedSpilled => OrderedVersionKind::CommittedSpilled,
            CapturedRowKind::PendingSecondaryHeap => OrderedVersionKind::PendingSecondaryHeap,
            CapturedRowKind::CommittedSecondaryHeap => OrderedVersionKind::CommittedSecondaryHeap,
            CapturedRowKind::Invisible => return Ok(None),
            CapturedRowKind::LookupSaved => {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "captured row metadata is incomplete"
                ));
            }
        };
        OrderedRowSnapshot {
            source: OrderedRowSource::new(self, kind),
            rowid: row.rowid,
            version: row.version,
            offset_or_member: row.offset_or_member,
            len: row.len,
        }
        .read()
        .map(Some)
    }

    pub(super) fn read_saved_snapshot(
        &self,
        rowid: u64,
        snapshot: RowSnapshot,
    ) -> Result<VisibleRowHome, SqlError> {
        let RowVersionIdentity::Committed(_) = snapshot.version else {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                "captured pending row is absent from its resident source"
            ));
        };
        if snapshot.created_at != self.created_at {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                "captured row belongs to a different table incarnation"
            ));
        }
        let read_position = self
            .roots
            .with_view(|source| Ok(source.commit_lsn()))?
            .ok_or_else(|| {
                sql_err!(
                    crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                    "captured row has no saved-file source"
                )
            })?;
        let mut row = CapturedRowRecord::empty(rowid, CapturedRowKind::LookupSaved);
        self.resolve_saved(&mut row, read_position)?;
        let home = self.read_record(row)?.ok_or_else(|| {
            sql_err!(
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                "captured saved row is no longer available"
            )
        })?;
        if home.snapshot().version != snapshot.version {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                "captured saved row has a different version"
            ));
        }
        Ok(home)
    }

    pub(super) fn roots(&self) -> &CapturedSpillRoots<'arena> {
        &self.roots
    }

    pub(super) fn for_each_saved_home(
        &self,
        rowids: &super::RowScanIds<'_>,
        each: &mut dyn FnMut(u64, VisibleRowHome) -> Result<core::ops::ControlFlow<()>, SqlError>,
    ) -> Result<core::ops::ControlFlow<()>, SqlError> {
        let Some(roots) = self.roots.owned_source_in(&self.pool)? else {
            return Ok(core::ops::ControlFlow::Continue(()));
        };
        let mut stopped = false;
        super::Storage::spill_merged_walk(&roots, rowids, &mut |rowid, state| {
            crate::sql::query::check_timeout()?;
            let home = state.into_captured_read()?;
            let result = each(rowid, VisibleRowHome::spilled(home))?;
            stopped = result.is_break();
            Ok(result)
        })?;
        Ok(if stopped {
            core::ops::ControlFlow::Break(())
        } else {
            core::ops::ControlFlow::Continue(())
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
            OrderedVersionKind::PendingPrimaryHeap | OrderedVersionKind::CommittedPrimaryHeap => {
                (1, 0, self.offset_or_member)
            }
            OrderedVersionKind::PendingSecondaryHeap
            | OrderedVersionKind::CommittedSecondaryHeap => (2, 0, self.offset_or_member),
        }
    }

    pub(crate) fn read(self) -> Result<VisibleRowHome, SqlError> {
        let source = self.source.owner();
        match self.source.kind() {
            OrderedVersionKind::PendingPrimaryHeap
            | OrderedVersionKind::CommittedPrimaryHeap
            | OrderedVersionKind::PendingSecondaryHeap
            | OrderedVersionKind::CommittedSecondaryHeap => {
                let heap = source
                    .heap
                    .as_ref()
                    .expect("retained heap row has an owner");
                let version = match self.source.kind() {
                    OrderedVersionKind::PendingPrimaryHeap
                    | OrderedVersionKind::PendingSecondaryHeap => {
                        RowVersionIdentity::Pending(self.version)
                    }
                    OrderedVersionKind::CommittedPrimaryHeap
                    | OrderedVersionKind::CommittedSecondaryHeap => {
                        RowVersionIdentity::Committed(self.version)
                    }
                    OrderedVersionKind::CommittedSpilled => unreachable!("heap row kind"),
                };
                let slot = match self.source.kind() {
                    OrderedVersionKind::PendingPrimaryHeap
                    | OrderedVersionKind::CommittedPrimaryHeap => HeapReadSlot::Primary,
                    OrderedVersionKind::PendingSecondaryHeap
                    | OrderedVersionKind::CommittedSecondaryHeap => HeapReadSlot::Secondary,
                    OrderedVersionKind::CommittedSpilled => unreachable!("heap row kind"),
                };
                let location = RowLoc {
                    offset: self.offset_or_member,
                    len: self.len,
                    generation: heap.generation(slot)?,
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
                    super::spill_generations::SpillGenerationId::from_record(self.offset_or_member),
                    self.version,
                )
                .map(VisibleRowHome::spilled)
            }
        }
    }
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
                OrderedVersionKind::PendingPrimaryHeap,
                OrderedVersionKind::CommittedPrimaryHeap,
                OrderedVersionKind::CommittedSpilled,
                OrderedVersionKind::PendingSecondaryHeap,
                OrderedVersionKind::CommittedSecondaryHeap,
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
    fn owned_ordered_rows_accept_moved_images_and_reject_foreign_owners() {
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
        storage.heap.append_bytes(b"dead").unwrap();
        let location = storage.heap.append_bytes(b"keep").unwrap();
        let foreign = super::super::row_heap::RowHeap::new(
            &mut Budget::new(super::super::row_heap::RowHeap::control_bytes() + 64),
            32,
        )
        .unwrap();
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
            let location = storage.heap.append_bytes(b"next").unwrap();
            assert_eq!(location.offset, retained.heap_loc().unwrap().offset);
            let moved = VisibleRowHome::heap(
                RowSnapshot {
                    created_at: source.created_at,
                    version: RowVersionIdentity::Committed(7),
                },
                storage.heap.get(location).unwrap(),
                table,
                1,
            );
            for version in [
                RowVersionIdentity::Committed(7),
                RowVersionIdentity::Pending(19),
            ] {
                let image = VisibleRowHome::heap(
                    RowSnapshot {
                        created_at: source.created_at,
                        version,
                    },
                    storage.heap.get(location).unwrap(),
                    table,
                    1,
                );
                let captured = source.capture_image(1, &image).unwrap();
                let restored = source.read_record(captured).unwrap().unwrap();
                assert_eq!(restored.snapshot().version, version);
                let VisibleRowBytes::Heap(bytes) = restored.bytes else {
                    panic!("heap fixture");
                };
                assert_eq!(&*bytes, b"next");
            }
            let moved_record = source.retain(1, &moved).unwrap();
            let VisibleRowBytes::Heap(bytes) = moved_record.read().unwrap().bytes else {
                panic!("heap fixture");
            };
            assert_eq!(&*bytes, b"next");
            let foreign = VisibleRowHome::heap(
                moved.snapshot(),
                foreign.append_image(b"lost").unwrap(),
                table,
                1,
            );
            assert_eq!(
                source.retain(1, &foreign).err().unwrap().sqlstate,
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
        let spilled = VisibleRowHome::spilled(
            storage
                .retain_spilled_row(
                    slot,
                    u64::MAX,
                    4,
                    crate::storage::SpillGenerationId::fixture(0),
                    4,
                )
                .unwrap(),
        );
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
