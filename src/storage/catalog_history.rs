//! Before-images of changed catalog rows, reserved before commit durability.

use core::hash::Hasher;
use core::mem::size_of;
use core::num::{NonZeroU32, NonZeroU64};
use std::sync::Arc;

use super::{DatabaseOid, Storage};
use crate::mem::arena::Arena;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::buffer::FixedBuf;
use crate::mem::fixed_map::Fnv1aHasher;
use crate::mem::fixed_vec::FixedVec;
use crate::mem::mutex::Mutex;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

const HEADER_BYTES: usize = 48;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CatalogPublication {
    /// The exclusive SQL worker admits no new statement before installation.
    Uninterrupted,
    MayYield,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CatalogRelationIdentity {
    pub(crate) database: Option<DatabaseOid>,
    pub(crate) relation: NonZeroU32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CatalogRowImage<'a> {
    pub(crate) bytes: &'a [u8],
    pub(crate) count: NonZeroU32,
    /// Updates retain their index root; delete followed by insert gets a new one.
    pub(crate) root: Option<NonZeroU64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HistoricalCatalogRow<'a> {
    pub(crate) key: &'a [u8],
    /// None means the object did not exist before this committed change.
    pub(crate) before: Option<CatalogRowImage<'a>>,
}

struct HistoryState {
    bytes: FixedBuf,
    reservations: FixedVec<NonZeroU64>,
    next_reservation: u64,
    max_versions_per_key: usize,
    latest_publication: u64,
}

pub(crate) struct CatalogHistory {
    state: Mutex<HistoryState>,
}

/// A unique reservation moves with its transaction, including queue handoff.
/// Dropping it before publication removes all of its unpublished images.
#[must_use = "catalog history must publish with its transaction or roll back"]
pub(crate) struct CatalogHistoryBatch {
    history: Arc<CatalogHistory>,
    identity: NonZeroU64,
    finished: bool,
    rows_present: bool,
}

#[derive(Clone, Copy)]
struct Frame<'a> {
    relation: CatalogRelationIdentity,
    batch: NonZeroU64,
    committed_at: u64,
    row: HistoricalCatalogRow<'a>,
}

fn frames(bytes: &[u8]) -> impl Iterator<Item = Frame<'_>> {
    let mut offset = 0;
    core::iter::from_fn(move || {
        if offset == bytes.len() {
            return None;
        }
        let header = &bytes[offset..offset + HEADER_BYTES];
        let length = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        let database = i32::from_le_bytes(header[4..8].try_into().unwrap());
        let relation = u32::from_le_bytes(header[8..12].try_into().unwrap());
        let count = u32::from_le_bytes(header[12..16].try_into().unwrap());
        let batch = u64::from_le_bytes(header[16..24].try_into().unwrap());
        let committed_at = u64::from_le_bytes(header[24..32].try_into().unwrap());
        let key_length = u32::from_le_bytes(header[32..36].try_into().unwrap()) as usize;
        let row_length = u32::from_le_bytes(header[36..40].try_into().unwrap()) as usize;
        let root = NonZeroU64::new(u64::from_le_bytes(header[40..48].try_into().unwrap()));
        assert_eq!(length, HEADER_BYTES + key_length + row_length);
        let key_start = offset + HEADER_BYTES;
        let row_start = key_start + key_length;
        let row_end = row_start + row_length;
        let frame = Frame {
            relation: CatalogRelationIdentity {
                database: (database != 0).then(|| {
                    DatabaseOid::parse(database).expect("catalog history database identity")
                }),
                relation: NonZeroU32::new(relation).expect("catalog history relation identity"),
            },
            batch: NonZeroU64::new(batch).expect("catalog history reservation identity"),
            committed_at,
            row: HistoricalCatalogRow {
                key: &bytes[key_start..row_start],
                before: NonZeroU32::new(count).map(|count| CatalogRowImage {
                    bytes: &bytes[row_start..row_end],
                    count,
                    root,
                }),
            },
        };
        offset = row_end;
        Some(frame)
    })
}

fn history_full(capacity: usize) -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "catalog_history_bytes exhausted (capacity {})",
        capacity
    )
}

impl HistoryState {
    fn remove(&mut self, mut discard: impl FnMut(Frame<'_>) -> bool) {
        // Offsets never escape the owner. Compaction cannot invalidate a
        // transaction reservation or a query's copied statement-arena rows.
        let mut destination = 0;
        let mut source = 0;
        while source < self.bytes.len() {
            let frame = frames(&self.bytes.readable()[source..])
                .next()
                .expect("history frame at the scan frontier");
            let length = HEADER_BYTES
                + frame.row.key.len()
                + frame.row.before.map_or(0, |before| before.bytes.len());
            if !discard(frame) {
                self.bytes
                    .filled_mut()
                    .copy_within(source..source + length, destination);
                destination += length;
            }
            source += length;
        }
        self.bytes.truncate_to(destination);
    }

    fn release(&mut self, identity: NonZeroU64) {
        let slot = self
            .reservations
            .iter()
            .position(|candidate| *candidate == identity)
            .expect("catalog history batch owns a live reservation");
        self.reservations.swap_remove(slot);
    }
}

impl CatalogHistory {
    pub(crate) fn budget_bytes(bytes: usize, transactions: usize) -> usize {
        size_of::<Arc<Self>>()
            .saturating_add(size_of::<Self>())
            .saturating_add(2 * size_of::<core::sync::atomic::AtomicUsize>())
            .saturating_add(bytes)
            .saturating_add(transactions.saturating_mul(size_of::<NonZeroU64>()))
    }

    pub(crate) fn new(
        budget: &mut Budget,
        bytes: usize,
        transactions: usize,
        max_versions_per_key: usize,
    ) -> Result<Arc<Self>, BudgetError> {
        budget.draw(Self::budget_bytes(0, 0), "catalog history owner")?;
        let bytes = FixedBuf::new(budget, "catalog history bytes", bytes)?;
        let reservations = FixedVec::new(
            budget,
            "catalog history transaction reservations",
            transactions,
        )?;
        Ok(Arc::new(Self {
            state: Mutex::new(HistoryState {
                bytes,
                reservations,
                next_reservation: 0,
                max_versions_per_key,
                latest_publication: 0,
            }),
        }))
    }

    pub(crate) fn reserve(self: &Arc<Self>) -> Result<CatalogHistoryBatch, SqlError> {
        let mut state = self.state.lock().expect("catalog history lock poisoned");
        let identity = state
            .next_reservation
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "catalog history reservation identity space exhausted"
                )
            })?;
        state
            .reservations
            .push(identity)
            .map_err(|error| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "{}", error))?;
        state.next_reservation = identity.get();
        Ok(CatalogHistoryBatch {
            history: Arc::clone(self),
            identity,
            finished: false,
            rows_present: false,
        })
    }

    /// Copy the earliest committed before-image after the selected snapshot
    /// for each changed key. A later catalog update must not replace it.
    pub(crate) fn rows_at<'a>(
        &self,
        relation: CatalogRelationIdentity,
        snapshot: u64,
        arena: &'a Arena,
    ) -> Result<&'a [HistoricalCatalogRow<'a>], SqlError> {
        self.rows_matching(relation, snapshot, |_| Ok(true), arena)
    }

    /// Filter before copying so repeated point lookups do not copy an entire
    /// catalog into statement memory. The predicate runs under the RAM owner.
    pub(crate) fn rows_matching<'a>(
        &self,
        relation: CatalogRelationIdentity,
        snapshot: u64,
        matches: impl Fn(&[u8]) -> Result<bool, SqlError>,
        arena: &'a Arena,
    ) -> Result<&'a [HistoricalCatalogRow<'a>], SqlError> {
        let state = self.state.lock().expect("catalog history lock poisoned");
        if snapshot >= state.latest_publication {
            return Ok(&[]);
        }
        let eligible =
            |frame: &Frame<'_>| frame.relation == relation && frame.committed_at > snapshot;
        let mut candidates = 0;
        for frame in frames(state.bytes.readable()).filter(eligible) {
            candidates += usize::from(matches(frame.row.key)?);
        }
        if candidates == 0 {
            return Ok(&[]);
        }
        let slots = candidates
            .checked_mul(2)
            .and_then(usize::checked_next_power_of_two)
            .ok_or_else(|| history_full(state.bytes.capacity()))?;
        let selected = arena
            .alloc_slice_with(slots, |_| None::<Frame<'_>>)
            .map_err(|_| crate::sql::eval::arena_full())?;
        let mut count = 0;
        for frame in frames(state.bytes.readable()).filter(eligible) {
            if !matches(frame.row.key)? {
                continue;
            }
            let mut hash = Fnv1aHasher::default();
            hash.write(frame.row.key);
            let mut slot = hash.finish() as usize & (slots - 1);
            loop {
                match selected[slot] {
                    Some(previous) if previous.row.key != frame.row.key => {
                        slot = (slot + 1) & (slots - 1);
                    }
                    Some(previous) => {
                        if frame.committed_at < previous.committed_at {
                            selected[slot] = Some(frame);
                        }
                        break;
                    }
                    None => {
                        selected[slot] = Some(frame);
                        count += 1;
                        break;
                    }
                }
            }
        }
        let output = arena
            .alloc_slice_with(count, |_| HistoricalCatalogRow {
                key: &[],
                before: None,
            })
            .map_err(|_| crate::sql::eval::arena_full())?;
        for (row, frame) in output.iter_mut().zip(selected.iter().flatten()) {
            row.key = arena
                .alloc_slice_copy(frame.row.key)
                .map_err(|_| crate::sql::eval::arena_full())?;
            row.before = frame
                .row
                .before
                .map(|before| {
                    Ok(CatalogRowImage {
                        bytes: arena
                            .alloc_slice_copy(before.bytes)
                            .map_err(|_| crate::sql::eval::arena_full())?,
                        count: before.count,
                        root: before.root,
                    })
                })
                .transpose()?;
        }
        Ok(output)
    }

    /// A statement copies its rows before releasing this owner; surviving
    /// transactions and exported snapshots supply the retention floor.
    pub(crate) fn reclaim(&self, oldest_snapshot: Option<u64>) {
        let mut state = self.state.lock().expect("catalog history lock poisoned");
        state.remove(|frame| {
            frame.committed_at != 0
                && oldest_snapshot.is_none_or(|oldest| frame.committed_at <= oldest)
        });
    }
}

impl Storage {
    pub(crate) fn reserve_catalog_history(
        &self,
        transaction: u32,
        publication: CatalogPublication,
    ) -> Result<Option<CatalogHistoryBatch>, SqlError> {
        if self.transaction_mode == crate::config::TransactionMode::Locking {
            return Ok(None);
        }
        let snapshots = self.snapshot_state();
        let required =
            publication == CatalogPublication::MayYield || snapshots.retains_for(transaction);
        let oldest = snapshots.oldest();
        drop(snapshots);
        self.catalog_history.reclaim(oldest);
        if required {
            self.catalog_history.reserve().map(Some)
        } else {
            Ok(None)
        }
    }

    pub(crate) fn catalog_rows_at<'a>(
        &self,
        relation: CatalogRelationIdentity,
        snapshot: u64,
        arena: &'a Arena,
    ) -> Result<&'a [HistoricalCatalogRow<'a>], SqlError> {
        self.catalog_history.rows_at(relation, snapshot, arena)
    }

    pub(crate) fn catalog_rows_matching<'a>(
        &self,
        relation: CatalogRelationIdentity,
        snapshot: u64,
        matches: impl Fn(&[u8]) -> Result<bool, SqlError>,
        arena: &'a Arena,
    ) -> Result<&'a [HistoricalCatalogRow<'a>], SqlError> {
        self.catalog_history
            .rows_matching(relation, snapshot, matches, arena)
    }
}

impl CatalogHistoryBatch {
    pub(crate) fn has_rows(&self) -> bool {
        self.rows_present
    }
    /// Visitors may copy into reserved RAM buffers; they must not perform I/O
    /// or call another history operation while the row owner is held.
    pub(crate) fn visit_rows(
        &self,
        mut visit: impl FnMut(CatalogRelationIdentity, HistoricalCatalogRow<'_>) -> Result<(), SqlError>,
    ) -> Result<(), SqlError> {
        let state = self
            .history
            .state
            .lock()
            .expect("catalog history lock poisoned");
        for frame in frames(state.bytes.readable()).filter(|frame| frame.batch == self.identity) {
            visit(frame.relation, frame.row)?;
        }
        Ok(())
    }

    pub(crate) fn record(
        &mut self,
        relation: CatalogRelationIdentity,
        key: &[u8],
        before: Option<CatalogRowImage<'_>>,
    ) -> Result<(), SqlError> {
        if key.is_empty()
            || before.is_some_and(|row| {
                row.bytes.is_empty() || (row.root.is_some() && row.count.get() != 1)
            })
        {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "catalog history requires an encoded key, nonempty images and unique tuple roots"
            ));
        }
        let mut state = self
            .history
            .state
            .lock()
            .expect("catalog history lock poisoned");
        let mut versions = 0;
        for frame in frames(state.bytes.readable()) {
            if frame.relation != relation || frame.row.key != key {
                continue;
            }
            if frame.batch == self.identity {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "catalog history batch contains a duplicate row key"
                ));
            }
            versions += 1;
        }
        if versions >= state.max_versions_per_key {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "catalog row exceeds max_catalog_versions_per_object ({})",
                state.max_versions_per_key
            ));
        }
        let row = before.map_or(&[][..], |row| row.bytes);
        let length = HEADER_BYTES
            .checked_add(key.len())
            .and_then(|length| length.checked_add(row.len()))
            .and_then(|length| u32::try_from(length).ok())
            .ok_or_else(|| history_full(state.bytes.capacity()))?;
        if length as usize > state.bytes.capacity() - state.bytes.len() {
            return Err(history_full(state.bytes.capacity()));
        }
        let mut header = [0; HEADER_BYTES];
        header[..4].copy_from_slice(&length.to_le_bytes());
        header[4..8].copy_from_slice(&relation.database.map_or(0, DatabaseOid::get).to_le_bytes());
        header[8..12].copy_from_slice(&relation.relation.get().to_le_bytes());
        header[12..16].copy_from_slice(&before.map_or(0, |row| row.count.get()).to_le_bytes());
        header[16..24].copy_from_slice(&self.identity.get().to_le_bytes());
        header[32..36].copy_from_slice(&(key.len() as u32).to_le_bytes());
        header[36..40].copy_from_slice(&(row.len() as u32).to_le_bytes());
        header[40..48].copy_from_slice(
            &before
                .and_then(|row| row.root)
                .map_or(0, NonZeroU64::get)
                .to_le_bytes(),
        );
        assert!(state.bytes.append(&header));
        assert!(state.bytes.append(key));
        assert!(state.bytes.append(row));
        self.rows_present = true;
        Ok(())
    }

    pub(crate) fn publish(mut self, committed_at: NonZeroU64) {
        let mut state = self
            .history
            .state
            .lock()
            .expect("catalog history lock poisoned");
        let mut offset = 0;
        let mut changed = false;
        while offset < state.bytes.len() {
            let frame = frames(&state.bytes.readable()[offset..])
                .next()
                .expect("catalog history frame at publication frontier");
            let length = HEADER_BYTES
                + frame.row.key.len()
                + frame.row.before.map_or(0, |before| before.bytes.len());
            if frame.batch == self.identity {
                assert_eq!(frame.committed_at, 0);
                state.bytes.filled_mut()[offset + 24..offset + 32]
                    .copy_from_slice(&committed_at.get().to_le_bytes());
                changed = true;
            }
            offset += length;
        }
        if changed {
            state.latest_publication = state.latest_publication.max(committed_at.get());
        }
        state.release(self.identity);
        self.finished = true;
    }
}

impl Drop for CatalogHistoryBatch {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut state = self
            .history
            .state
            .lock()
            .expect("catalog history lock poisoned");
        state.remove(|frame| frame.batch == self.identity);
        state.release(self.identity);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::guard::forbid_alloc;

    fn relation(database: DatabaseOid, oid: u32) -> CatalogRelationIdentity {
        CatalogRelationIdentity {
            database: Some(database),
            relation: NonZeroU32::new(oid).unwrap(),
        }
    }

    fn image(bytes: &[u8]) -> Option<CatalogRowImage<'_>> {
        Some(CatalogRowImage {
            bytes,
            count: NonZeroU32::new(1).unwrap(),
            root: None,
        })
    }

    #[test]
    fn catalog_history_filtered_lookup_bounds_copies_and_propagates_key_errors() {
        let mut budget = Budget::new(16 * 1024);
        let history = CatalogHistory::new(&mut budget, 4096, 1, 8).unwrap();
        let arena = Arena::new(&mut budget, "catalog point lookup", 256).unwrap();
        let table = relation(DatabaseOid::POSTGRES, 2609);
        forbid_alloc(|| {
            let mut batch = history.reserve().unwrap();
            for key in 0u8..12 {
                batch.record(table, &[key], image(&[42; 128])).unwrap();
            }
            batch.record(table, b"target", image(b"old")).unwrap();
            batch.publish(NonZeroU64::new(7).unwrap());
            let rows = history
                .rows_matching(table, 6, |key| Ok(key == b"target"), &arena)
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].before, image(b"old"));
            let error = history
                .rows_matching(
                    table,
                    6,
                    |_| {
                        Err(sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "invalid point lookup key"
                        ))
                    },
                    &arena,
                )
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::INTERNAL_ERROR);
            history.reclaim(None);
            assert_eq!(rows[0].before, image(b"old"));
        });
    }

    #[test]
    fn catalog_history_preserves_oldest_future_row_and_creation_absence() {
        let mut budget = Budget::new(32 * 1024);
        let history = CatalogHistory::new(&mut budget, 4096, 2, 8).unwrap();
        let arena = Arena::new(&mut budget, "catalog history test", 8192).unwrap();
        let table = relation(DatabaseOid::POSTGRES, 1259);
        forbid_alloc(|| {
            let mut first = history.reserve().unwrap();
            first
                .record(table, b"orders", image(b"old columns"))
                .unwrap();
            first.record(table, b"created", None).unwrap();
            assert!(history.rows_at(table, 1, &arena).unwrap().is_empty());
            first.publish(NonZeroU64::new(7).unwrap());
            let mut second = history.reserve().unwrap();
            second
                .record(table, b"orders", image(b"new columns"))
                .unwrap();
            second.publish(NonZeroU64::new(9).unwrap());
            let rows = history.rows_at(table, 6, &arena).unwrap();
            assert_eq!(rows.len(), 2);
            assert_eq!(
                rows.iter().find(|row| row.key == b"orders").unwrap().before,
                image(b"old columns")
            );
            assert_eq!(
                rows.iter()
                    .find(|row| row.key == b"created")
                    .unwrap()
                    .before,
                None
            );
            let rows = history.rows_at(table, 7, &arena).unwrap();
            assert_eq!(
                rows,
                &[HistoricalCatalogRow {
                    key: b"orders",
                    before: image(b"new columns")
                }]
            );
            assert!(history.rows_at(table, 9, &arena).unwrap().is_empty());
        });
    }

    #[test]
    fn catalog_history_scopes_rows_by_database_and_relation() {
        let mut budget = Budget::new(32 * 1024);
        let history = CatalogHistory::new(&mut budget, 4096, 1, 8).unwrap();
        let arena = Arena::new(&mut budget, "catalog history test", 8192).unwrap();
        forbid_alloc(|| {
            let mut batch = history.reserve().unwrap();
            for (identity, value) in [
                (
                    relation(DatabaseOid::POSTGRES, 1259),
                    &b"postgres class"[..],
                ),
                (
                    relation(DatabaseOid::TEMPLATE1, 1259),
                    &b"template class"[..],
                ),
                (
                    relation(DatabaseOid::POSTGRES, 1249),
                    &b"postgres attribute"[..],
                ),
                (
                    CatalogRelationIdentity {
                        database: None,
                        relation: NonZeroU32::new(1260).unwrap(),
                    },
                    &b"shared role"[..],
                ),
            ] {
                batch.record(identity, b"same key", image(value)).unwrap();
            }
            batch.publish(NonZeroU64::new(7).unwrap());
            let rows = history
                .rows_at(relation(DatabaseOid::POSTGRES, 1259), 6, &arena)
                .unwrap();
            assert_eq!(
                rows,
                &[HistoricalCatalogRow {
                    key: b"same key",
                    before: image(b"postgres class")
                }]
            );
            let shared = history
                .rows_at(
                    CatalogRelationIdentity {
                        database: None,
                        relation: NonZeroU32::new(1260).unwrap(),
                    },
                    6,
                    &arena,
                )
                .unwrap();
            assert_eq!(shared[0].before, image(b"shared role"));
        });
    }

    #[test]
    fn catalog_history_rollback_and_reclamation_preserve_copied_rows() {
        let bytes = HEADER_BYTES + b"key".len() + b"old".len();
        let mut budget = Budget::new(8192);
        let history = CatalogHistory::new(&mut budget, bytes * 2, 2, 8).unwrap();
        let arena = Arena::new(&mut budget, "catalog history test", 4096).unwrap();
        let table = relation(DatabaseOid::POSTGRES, 1259);
        forbid_alloc(|| {
            let mut pending = history.reserve().unwrap();
            pending.record(table, b"key", image(b"old")).unwrap();
            let mut committed = history.reserve().unwrap();
            committed.record(table, b"key", image(b"new")).unwrap();
            committed.publish(NonZeroU64::new(7).unwrap());
            let rows = history.rows_at(table, 6, &arena).unwrap();
            drop(pending);
            history.reclaim(Some(6));
            assert_eq!(history.rows_at(table, 6, &arena).unwrap(), rows);
            history.reclaim(Some(7));
            assert!(history.rows_at(table, 6, &arena).unwrap().is_empty());
            assert_eq!(rows[0].before, image(b"new"));
            let mut reused = history.reserve().unwrap();
            reused.record(table, b"key", image(b"old")).unwrap();
            history.reclaim(None);
            reused.publish(NonZeroU64::new(9).unwrap());
            assert_eq!(
                history.rows_at(table, 8, &arena).unwrap()[0].before,
                image(b"old")
            );
            history.reclaim(None);
            assert!(history.rows_at(table, 8, &arena).unwrap().is_empty());
        });
    }

    #[test]
    fn catalog_history_rejects_capacity_and_duplicate_keys_without_partial_records() {
        let bytes = HEADER_BYTES + b"key".len() + b"old".len();
        let mut budget = Budget::new(8192);
        let before = budget.remaining();
        let history = CatalogHistory::new(&mut budget, bytes, 1, 8).unwrap();
        assert_eq!(
            before - budget.remaining(),
            CatalogHistory::budget_bytes(bytes, 1)
        );
        let arena = Arena::new(&mut budget, "catalog history test", 4096).unwrap();
        let table = relation(DatabaseOid::POSTGRES, 1259);
        forbid_alloc(|| {
            let mut batch = history.reserve().unwrap();
            assert!(history.reserve().is_err());
            batch.record(table, b"key", image(b"old")).unwrap();
            assert_eq!(
                batch
                    .record(table, b"key", image(b"other"))
                    .unwrap_err()
                    .sqlstate,
                sqlstate::INTERNAL_ERROR
            );
            let error = batch.record(table, b"new", None).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(error.message.as_str().contains("catalog_history_bytes"));
            batch.publish(NonZeroU64::new(7).unwrap());
            assert_eq!(
                history.rows_at(table, 6, &arena).unwrap(),
                &[HistoricalCatalogRow {
                    key: b"key",
                    before: image(b"old")
                }]
            );
            history.reclaim(None);
            let mut empty = history.reserve().unwrap();
            assert!(empty.record(table, &[], None).is_err());
            assert!(empty.record(table, b"key", image(&[])).is_err());
            empty.publish(NonZeroU64::new(9).unwrap());
        });
    }

    #[test]
    fn catalog_history_version_limit_releases_only_after_the_retention_floor() {
        let mut budget = Budget::new(16 * 1024);
        let history = CatalogHistory::new(&mut budget, 4096, 2, 1).unwrap();
        let arena = Arena::new(&mut budget, "catalog history test", 8192).unwrap();
        let table = relation(DatabaseOid::POSTGRES, 1259);
        forbid_alloc(|| {
            let mut first = history.reserve().unwrap();
            first.record(table, b"key", image(b"old")).unwrap();
            first.publish(NonZeroU64::new(7).unwrap());
            let mut second = history.reserve().unwrap();
            for floor in [None, Some(6)] {
                if floor.is_some() {
                    history.reclaim(floor);
                }
                let error = second.record(table, b"key", image(b"new")).unwrap_err();
                assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
                assert!(
                    error
                        .message
                        .as_str()
                        .contains("max_catalog_versions_per_object")
                );
                assert_eq!(
                    history.rows_at(table, 6, &arena).unwrap()[0].before,
                    image(b"old")
                );
            }
            history.reclaim(Some(7));
            second.record(table, b"key", image(b"new")).unwrap();
            second.publish(NonZeroU64::new(9).unwrap());
            assert_eq!(
                history.rows_at(table, 7, &arena).unwrap()[0].before,
                image(b"new")
            );
        });
    }

    #[test]
    fn catalog_history_binary_key_collisions_and_duplicate_row_counts_are_exact() {
        let mut budget = Budget::new(16 * 1024);
        let history = CatalogHistory::new(&mut budget, 4096, 2, 8).unwrap();
        let arena = Arena::new(&mut budget, "catalog history test", 8192).unwrap();
        let table = relation(DatabaseOid::POSTGRES, 2608);
        forbid_alloc(|| {
            let mut first = history.reserve().unwrap();
            let duplicates = CatalogRowImage {
                bytes: b"dependency",
                count: NonZeroU32::new(3).unwrap(),
                root: None,
            };
            let mut invalid = duplicates;
            invalid.root = NonZeroU64::new(17);
            assert_eq!(
                first
                    .record(table, &[0], Some(invalid))
                    .unwrap_err()
                    .sqlstate,
                sqlstate::INTERNAL_ERROR
            );
            assert!(!first.has_rows());
            first.record(table, &[0], Some(duplicates)).unwrap();
            let rooted = Some(CatalogRowImage {
                bytes: b"unique",
                count: NonZeroU32::new(1).unwrap(),
                root: NonZeroU64::new(17),
            });
            first.record(table, &[16], rooted).unwrap();
            first.record(table, &[32], None).unwrap();
            first.publish(NonZeroU64::new(7).unwrap());
            let rows = history.rows_at(table, 6, &arena).unwrap();
            assert_eq!(rows.len(), 3);
            assert_eq!(
                rows.iter().find(|row| row.key == [0]).unwrap().before,
                Some(duplicates)
            );
            assert_eq!(
                rows.iter().find(|row| row.key == [16]).unwrap().before,
                rooted
            );
            assert_eq!(
                rows.iter().find(|row| row.key == [32]).unwrap().before,
                None
            );
        });
    }

    #[test]
    fn catalog_history_exhausted_identity_and_statement_arena_are_named_errors() {
        let mut budget = Budget::new(8192);
        let history = CatalogHistory::new(&mut budget, 4096, 1, 8).unwrap();
        let arena = Arena::new(&mut budget, "small catalog history test", 1).unwrap();
        let table = relation(DatabaseOid::POSTGRES, 1259);
        forbid_alloc(|| {
            let mut batch = history.reserve().unwrap();
            batch.record(table, b"key", image(b"old")).unwrap();
            batch.publish(NonZeroU64::new(7).unwrap());
            let error = history.rows_at(table, 6, &arena).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(error.message.as_str().contains("SQL arena"));
            history.state.lock().unwrap().next_reservation = u64::MAX;
            let error = match history.reserve() {
                Ok(_) => panic!("catalog history reservation identity wrapped"),
                Err(error) => error,
            };
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(error.message.as_str().contains("identity space exhausted"));
        });
    }

    #[test]
    fn catalog_history_four_readers_see_atomic_publication_and_own_copied_rows() {
        let mut budget = Budget::new(256 * 1024);
        let history = CatalogHistory::new(&mut budget, 4096, 1, 8).unwrap();
        let arenas: [Arena; 4] = core::array::from_fn(|_| {
            Arena::new(&mut budget, "concurrent catalog history test", 32 * 1024).unwrap()
        });
        let rendezvous = Arc::new(std::sync::Barrier::new(5));
        let table = relation(DatabaseOid::POSTGRES, 1259);
        let batch = forbid_alloc(|| {
            let mut batch = history.reserve().unwrap();
            batch.record(table, b"orders", image(b"10")).unwrap();
            batch.record(table, b"invoices", image(b"10")).unwrap();
            batch
        });
        std::thread::scope(|scope| {
            for arena in arenas {
                let history = Arc::clone(&history);
                let rendezvous = Arc::clone(&rendezvous);
                scope.spawn(move || {
                    rendezvous.wait();
                    forbid_alloc(|| {
                        for _ in 0..32 {
                            let rows = history.rows_at(table, 6, &arena).unwrap();
                            assert!(rows.is_empty() || rows.len() == 2);
                            assert!(rows.iter().all(|row| row.before == image(b"10")));
                            std::thread::yield_now();
                        }
                    });
                    rendezvous.wait();
                    let copied = forbid_alloc(|| history.rows_at(table, 6, &arena).unwrap());
                    assert_eq!(copied.len(), 2);
                    rendezvous.wait();
                    rendezvous.wait();
                    forbid_alloc(|| {
                        assert!(copied.iter().all(|row| row.before == image(b"10")));
                        assert!(history.rows_at(table, 6, &arena).unwrap().is_empty());
                    });
                });
            }
            rendezvous.wait();
            forbid_alloc(|| batch.publish(NonZeroU64::new(7).unwrap()));
            rendezvous.wait();
            rendezvous.wait();
            forbid_alloc(|| history.reclaim(None));
            rendezvous.wait();
        });
    }

    #[test]
    fn catalog_history_unwinding_releases_reservations_and_unpublished_bytes() {
        let mut budget = Budget::new(8192);
        let history = CatalogHistory::new(&mut budget, HEADER_BYTES + 6, 1, 1).unwrap();
        let table = relation(DatabaseOid::POSTGRES, 1259);
        let batch = forbid_alloc(|| {
            let mut batch = history.reserve().unwrap();
            batch.record(table, b"key", image(b"old")).unwrap();
            batch
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _batch = batch;
            panic!("catalog preparation interrupted");
        }));
        assert!(result.is_err());
        forbid_alloc(|| {
            let mut replacement = history.reserve().unwrap();
            replacement.record(table, b"key", image(b"new")).unwrap();
            replacement.publish(NonZeroU64::new(7).unwrap());
        });
    }

    #[test]
    fn catalog_history_disabled_byte_pool_never_accepts_a_row() {
        let mut budget = Budget::new(8192);
        let history = CatalogHistory::new(&mut budget, 0, 1, 8).unwrap();
        let arena = Arena::new(&mut budget, "disabled catalog history test", 1).unwrap();
        let table = relation(DatabaseOid::POSTGRES, 1259);
        forbid_alloc(|| {
            assert!(history.rows_at(table, 0, &arena).unwrap().is_empty());
            let mut batch = history.reserve().unwrap();
            let error = batch.record(table, b"key", None).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(error.message.as_str().contains("catalog_history_bytes"));
            drop(batch);
            let empty = history.reserve().unwrap();
            empty.publish(NonZeroU64::new(7).unwrap());
            history.reclaim(None);
        });
    }
}
