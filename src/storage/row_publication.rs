//! Prepare row images without publication locks, then validate and publish.

use super::row_versions::RowVersionState;
use super::*;

struct PreparedPendingWrite {
    image: Option<super::row_heap::HeapRowRead>,
    target: PreparedTableTarget,
    rowid: u64,
    expected: Option<RowState>,
    pending_identity: Option<u64>,
    committed: Option<RowHome>,
    committed_lsn: u64,
    change: PendingChange,
    existed: bool,
    track_statistics: bool,
}

#[derive(Clone, Copy)]
struct PreparedTableTarget {
    table: usize,
    database: DatabaseOid,
    incarnation: u64,
}

impl PreparedTableTarget {
    fn same_target(self, other: Self) -> Result<(), SqlError> {
        if self.table != other.table
            || self.database != other.database
            || self.incarnation != other.incarnation
        {
            return Err(Self::retired_error());
        }
        Ok(())
    }

    fn retired_error() -> SqlError {
        sql_err!(
            sqlstate::SERIALIZATION_FAILURE,
            "pending row belongs to a retired table incarnation"
        )
    }

    fn capture(
        table: usize,
        identity: table_definitions::TableIdentity,
        txid: u32,
    ) -> Result<Self, SqlError> {
        let target = Self {
            table,
            database: identity.database,
            incarnation: identity.created_at,
        };
        target.validate(identity, txid)?;
        Ok(target)
    }

    fn validate(
        &self,
        identity: table_definitions::TableIdentity,
        txid: u32,
    ) -> Result<(), SqlError> {
        if identity.database != self.database
            || identity.created_at != self.incarnation
            || !identity.existence.visible_to(txid)
        {
            return Err(Self::retired_error());
        }
        Ok(())
    }
}

enum PendingPublication {
    Written(PendingWriteUndo),
    Retry,
    DefinitionBusy,
    Wait(u32, &'static str),
}

impl Storage {
    /// Input bytes remain owned from append through preparation and publication.
    pub(crate) fn write_pending_image(
        &self,
        table_index: usize,
        rowid: u64,
        txid: u32,
        cid: u32,
        image: Option<super::row_heap::HeapRowRead>,
    ) -> Result<PendingWriteUndo, SqlError> {
        self.write_pending_image_inner(table_index, rowid, txid, cid, image, true)
    }

    /// Rewrites and WAL reconstruction do not increment DML counts.
    pub(crate) fn write_pending_image_untracked(
        &self,
        table_index: usize,
        rowid: u64,
        txid: u32,
        cid: u32,
        image: Option<super::row_heap::HeapRowRead>,
    ) -> Result<PendingWriteUndo, SqlError> {
        self.write_pending_image_inner(table_index, rowid, txid, cid, image, false)
    }

    /// Fixture adapter; production callers supply owned images.
    #[cfg(test)]
    pub fn write_pending(
        &self,
        table_index: usize,
        rowid: u64,
        txid: u32,
        cid: u32,
        loc: Option<RowLoc>,
    ) -> Result<PendingWriteUndo, SqlError> {
        self.write_pending_inner(table_index, rowid, txid, cid, loc, true)
    }

    /// Fixture adapter for untracked reconstruction.
    #[cfg(test)]
    pub(crate) fn write_pending_untracked(
        &self,
        table_index: usize,
        rowid: u64,
        txid: u32,
        cid: u32,
        loc: Option<RowLoc>,
    ) -> Result<PendingWriteUndo, SqlError> {
        self.write_pending_inner(table_index, rowid, txid, cid, loc, false)
    }

    #[cfg(test)]
    pub(super) fn write_pending_inner(
        &self,
        table_index: usize,
        rowid: u64,
        txid: u32,
        cid: u32,
        loc: Option<RowLoc>,
        track_statistics: bool,
    ) -> Result<PendingWriteUndo, SqlError> {
        let image = loc.map(|location| self.heap.get(location)).transpose()?;
        self.write_pending_image_inner(table_index, rowid, txid, cid, image, track_statistics)
    }

    fn write_pending_image_inner(
        &self,
        table_index: usize,
        rowid: u64,
        txid: u32,
        cid: u32,
        mut image: Option<super::row_heap::HeapRowRead>,
        track_statistics: bool,
    ) -> Result<PendingWriteUndo, SqlError> {
        let mut original_target: Option<PreparedTableTarget> = None;
        loop {
            if let Some(target) = original_target {
                let definition = self.tables[table_index].definition.read();
                target.validate(definition.identity, txid)?;
            }
            if let Some(input) = &image {
                image = Some(self.heap.retain_for_publication(input)?);
            }
            let prepared = self.prepare_pending_write_image(
                table_index,
                rowid,
                txid,
                cid,
                image.as_ref(),
                track_statistics,
            )?;
            let target = prepared.target;
            if let Some(original) = original_target {
                original.same_target(target)?;
            } else {
                original_target = Some(target);
            }
            match self.publish_pending_write(prepared)? {
                PendingPublication::Written(undo) => return Ok(undo),
                PendingPublication::Retry => continue,
                PendingPublication::DefinitionBusy => {
                    // Wait outside the snapshot registry and retain the original
                    // request identity when byte preparation needs another turn.
                    let definition = self.tables[table_index].definition.read();
                    target.validate(definition.identity, txid)?;
                }
                PendingPublication::Wait(owner, message) => {
                    return Err(self.pending_write_wait(txid, owner, message));
                }
            }
        }
    }

    fn pending_write_wait(&self, txid: u32, owner: u32, message: &'static str) -> SqlError {
        match self.wait_for_transaction(txid, owner) {
            Err(error) => error,
            Ok(()) => sql_err!(sqlstate::INTERNAL_LOCK_WAIT, "{}", message),
        }
    }

    #[cfg(test)]
    fn prepare_pending_write(
        &self,
        table: usize,
        rowid: u64,
        txid: u32,
        cid: u32,
        loc: Option<RowLoc>,
        track_statistics: bool,
    ) -> Result<PreparedPendingWrite, SqlError> {
        let image = loc.map(|location| self.heap.get(location)).transpose()?;
        self.prepare_pending_write_image(table, rowid, txid, cid, image.as_ref(), track_statistics)
    }

    fn prepare_pending_write_image(
        &self,
        table: usize,
        rowid: u64,
        txid: u32,
        cid: u32,
        image: Option<&super::row_heap::HeapRowRead>,
        track_statistics: bool,
    ) -> Result<PreparedPendingWrite, SqlError> {
        // Own input bytes across cold reads and movement. Publication rechecks
        // placement under the same version ownership as relocation.
        let loc = image.map(super::row_heap::HeapRowRead::location);
        let target = {
            let definition = self.table_def(table, txid);
            if let Some(owner) = definition.locked_by_other(txid) {
                return Err(self.pending_write_wait(
                    txid,
                    owner,
                    "statement is waiting for a concurrent table definition change",
                ));
            }
            PreparedTableTarget::capture(table, definition.identity(), txid)?
        };
        let resident = self.resident_row_state(table, rowid)?;
        if let Some(owner) = resident
            .as_ref()
            .and_then(|state| state.locked_by_other(txid))
        {
            drop(resident);
            return Err(self.pending_write_wait(
                txid,
                owner,
                "statement is waiting for a concurrent row update",
            ));
        }
        let (expected, pending_identity, existed) = match resident.as_ref() {
            Some(state) => (
                Some(**state),
                state.pending_head_identity(),
                state
                    .visible_at(txid, SNAPSHOT_ALL, u64::MAX)
                    .flatten()
                    .is_some(),
            ),
            None => (None, None, false),
        };
        // No chain/map guard survives into a cold point read or comparison.
        let spilled = if expected.is_none() {
            self.spill_row_state_at(table, rowid, u64::MAX)?
        } else {
            None
        };
        let committed = expected.map_or_else(
            || spilled.as_ref().and_then(|source| source.state().committed),
            |state| state.committed,
        );
        let committed_lsn = expected.map_or_else(
            || {
                spilled
                    .as_ref()
                    .map_or(0, |source| source.state().committed_lsn)
            },
            |state| state.committed_lsn,
        );
        let committed_generation = expected.map_or_else(
            || {
                spilled
                    .as_ref()
                    .and_then(|source| source.state().committed_generation)
            },
            |state| state.committed_generation,
        );
        let changes_existence = committed.is_some() != image.is_some();
        let changed_columns = if track_statistics
            && self.tables[table].pending_definition_transaction() != Some(txid)
            && committed.is_some()
            && let Some(input) = image
        {
            let committed_image = match resident {
                Some(state) => self.committed_row_home(table, rowid, state)?,
                None => match spilled {
                    Some(source) => source.visible_at(u64::MAX)?.map(VisibleRowHome::spilled),
                    None => None,
                },
            }
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "committed update has no retained row source"
                )
            })?;
            self.pending_change_footprint(table, rowid, committed_image, input)?
        } else {
            let columns = self.tables[table].definition().n_columns;
            ColumnSet::all(columns)
        };
        Ok(PreparedPendingWrite {
            image: image.cloned(),
            target,
            rowid,
            expected,
            pending_identity,
            committed,
            committed_lsn,
            change: PendingChange {
                txid,
                cid,
                loc,
                changed_columns,
                changes_existence,
            },
            existed: existed || (expected.is_none() && committed.is_some()),
            track_statistics,
        })
    }

    fn publish_pending_write(
        &self,
        prepared: PreparedPendingWrite,
    ) -> Result<PendingPublication, SqlError> {
        let PreparedPendingWrite {
            image,
            target,
            rowid,
            expected,
            pending_identity,
            committed,
            committed_lsn,
            change,
            existed,
            track_statistics,
        } = prepared;
        let table = target.table;
        let owners = super::row_ownership::acquire(|| {
            let snapshots =
                super::row_ownership::try_mutex(&self.snapshots, "snapshot state lock poisoned")?;
            let definition = match self.tables[table].definition.try_read() {
                Ok(definition) => definition,
                Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    panic!("table definition lock poisoned")
                }
            };
            let versions = self.row_versions.try_write_for_batch()?;
            let rows = self.tables[table].rows.try_write_for_batch()?;
            Ok(Some((snapshots, definition, versions, rows)))
        });
        let Some((snapshots, definition, mut versions, mut rows)) = owners else {
            return Ok(PendingPublication::DefinitionBusy);
        };
        target.validate(definition.identity, change.txid)?;
        if let Some(owner) = definition
            .identity
            .existence
            .pending_txid()
            .or_else(|| definition.pending.map(|head| head.transaction))
            .filter(|&owner| owner != change.txid)
        {
            return Ok(PendingPublication::Wait(
                owner,
                "statement is waiting for a concurrent table definition change",
            ));
        }
        // Retention selection and history pruning share registry ownership.
        // A newly registered reader cannot appear between those operations.
        let oldest = snapshots.oldest();
        // Never perform byte or object reads while owning publication guards;
        // statistics failure is undone before releasing the selected version.
        // The owned bytes remain pinned until publication releases its owners.
        // Never put an old allocation's location into the current row maps.
        if image
            .as_ref()
            .is_some_and(|image| !self.heap.accepts_publication(image))
        {
            return Ok(PendingPublication::Retry);
        }
        let current = rows.get(&rowid).copied();
        if let Some(owner) = current.and_then(|state| {
            pending_last(&versions.pending_row_versions, state.pending)
                .filter(|head| head.txid != change.txid)
                .map(|head| head.txid)
        }) {
            return Ok(PendingPublication::Wait(
                owner,
                "statement is waiting for a concurrent row update",
            ));
        }
        let current_identity = current
            .and_then(|state| state.pending.tail)
            .map(|slot| versions.pending_row_versions[slot].identity);
        if current != expected || current_identity != pending_identity {
            return Ok(PendingPublication::Retry);
        }
        if current.is_none() && rows.len() == rows.capacity() {
            Self::evict_redundant_rows(&mut rows);
            if rows.len() == rows.capacity() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "table row limit reached ({} rows in memtable)",
                    rows.capacity()
                ));
            }
        }
        let mut state = current.unwrap_or(RowState {
            committed,
            committed_lsn,
            committed_generation,
            checkpoint_change_lsn: 0,
            history: CommittedHistory::empty(),
            pending: PendingVersions::empty(),
        });
        // Readers at or after the current image need no earlier boundary.
        let retained = oldest
            .filter(|snapshot| !state.has_committed_version() || *snapshot < state.committed_lsn);
        prune_committed_history(
            &mut versions.committed_row_versions,
            &mut state.history,
            retained,
        );
        // Pruning releases slots, so publish its chain handles even if a later
        // capacity check fails. Readers can never traverse a released chain.
        if current.is_some() {
            *rows.get_mut(&rowid).expect("validated row") = state;
        }
        if oldest.is_some()
            && state.pending.is_none()
            && state.has_committed_version()
            && state.history.len() == self.max_row_versions_per_row
        {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "active snapshot history for row {} reached max_row_versions_per_row ({})",
                rowid,
                self.max_row_versions_per_row
            ));
        }
        let undo = push_pending_version(
            &mut versions.pending_row_versions,
            &mut state.pending,
            self.max_row_versions_per_row,
            change,
        )?;
        rows.insert(rowid, state)
            .expect("map capacity reserved under its owner");
        if track_statistics
            && let Err(error) =
                self.record_relation_write(change.txid, table, existed, change.loc.is_some())
        {
            Self::restore_pending_locked(&mut versions, &mut rows, rowid, change.txid, undo);
            return Err(error);
        }
        Ok(PendingPublication::Written(undo))
    }

    /// Undo checks and removes one exact head under the same publication locks.
    pub fn restore_pending(&self, table: usize, rowid: u64, txid: u32, undo: PendingWriteUndo) {
        let (mut versions, mut rows) = super::row_ownership::acquire(|| {
            Ok((
                self.row_versions.try_write_for_batch()?,
                self.tables[table].rows.try_write_for_batch()?,
            ))
        });
        Self::restore_pending_locked(&mut versions, &mut rows, rowid, txid, undo);
    }

    fn restore_pending_locked(
        versions: &mut RowVersionState,
        rows: &mut RowEntries,
        rowid: u64,
        txid: u32,
        undo: PendingWriteUndo,
    ) {
        let Some(state) = rows.get_mut(&rowid) else {
            return;
        };
        let Some(slot) = state.pending.tail else {
            return;
        };
        let head = versions.pending_row_versions[slot];
        if head.identity != undo.identity || head.change.txid != txid {
            return;
        }
        pop_pending_version(&mut versions.pending_row_versions, &mut state.pending);
        // A committed deletion still shadows older immutable SST images.
        if (!state.has_committed_version() && state.history.is_empty() && state.pending.is_none())
            || Self::redundant_spilled_row_state(state)
        {
            rows.remove(&rowid);
        }
    }

    pub(super) fn evict_redundant_rows(rows: &mut RowEntries) {
        rows.retain(|_, state| !Self::redundant_spilled_row_state(state));
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{make_def, test_budget, test_config};
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    fn fixture(config: &Config) -> (Storage, usize) {
        let mut budget = test_budget(config);
        let mut storage = Storage::new(config, &mut budget).unwrap();
        let table = storage
            .create_table(make_def("publication", &[("id", ColType::Int4, false)]))
            .unwrap();
        (storage, table)
    }

    fn relocation_scratch() -> FixedVec<HeapRelocationEntry> {
        FixedVec::new(&mut Budget::new(4096), "publication relocation test", 16).unwrap()
    }

    #[test]
    fn heap_eviction_preserves_unpublished_heap_deleted_historical_and_pending_rows() {
        const REDUNDANT: usize = 1536;
        let mut config = test_config();
        config.table_rows = 2048;
        let (mut storage, table) = fixture(&config);
        crate::mem::guard::forbid_alloc(|| {
            let location = storage.heap.append_bytes(b"kept").unwrap();
            let spilled = RowState {
                committed: Some(RowHome::Spilled {
                    len: 4,
                    sst: 0,
                    commit_lsn: 7,
                }),
                committed_lsn: 7,
                committed_generation: Some(0),
                checkpoint_change_lsn: 0,
                history: CommittedHistory::empty(),
                pending: PendingVersions::empty(),
            };
            let mut retained = [spilled; 5];
            retained[0].checkpoint_change_lsn = 7;
            retained[1].committed = Some(RowHome::Heap(location));
            retained[2].committed = None;
            {
                let versions = storage.row_versions.exclusive();
                push_committed_version(
                    &mut versions.committed_row_versions,
                    &mut retained[3].history,
                    config.max_row_versions_per_row,
                    CommittedVersion {
                        home: Some(RowHome::Heap(location)),
                        lsn: 6,
                    },
                )
                .unwrap();
                push_pending_version(
                    &mut versions.pending_row_versions,
                    &mut retained[4].pending,
                    config.max_row_versions_per_row,
                    PendingChange {
                        txid: 9,
                        cid: 1,
                        loc: Some(location),
                        changed_columns: ColumnSet::EMPTY,
                        changes_existence: false,
                    },
                )
                .unwrap();
            }
            {
                let mut rows = storage.tables[table].rows.write();
                for rowid in 0..REDUNDANT {
                    rows.insert(rowid as u64, spilled).unwrap();
                }
                for (index, state) in retained.iter().enumerate() {
                    rows.insert((REDUNDANT + index) as u64, *state).unwrap();
                }
                Storage::evict_redundant_rows(&mut rows);
                assert_eq!(rows.len(), retained.len());
                for rowid in 0..REDUNDANT {
                    assert!(rows.get(&(rowid as u64)).is_none());
                }
                for (index, state) in retained.iter().enumerate() {
                    assert_eq!(rows.get(&((REDUNDANT + index) as u64)), Some(state));
                }
                Storage::evict_redundant_rows(&mut rows);
                assert_eq!(rows.len(), retained.len());
            }
            let historical = storage
                .resident_row_state(table, (REDUNDANT + 3) as u64)
                .unwrap()
                .unwrap();
            assert_eq!(
                historical.visible_at(0, SNAPSHOT_ALL, 6),
                Some(Some(RowHome::Heap(location)))
            );
            drop(historical);
            let pending = storage
                .resident_row_state(table, (REDUNDANT + 4) as u64)
                .unwrap()
                .unwrap();
            assert_eq!(
                pending.visible_at(9, SNAPSHOT_ALL, u64::MAX),
                Some(Some(RowHome::Heap(location)))
            );
        });
    }

    #[test]
    fn owned_write_inputs_mixed_allocations_publish_and_release_with_fixed_memory() {
        for tracked in [true, false] {
            let (mut storage, table) = fixture(&test_config());
            let mut scratch = relocation_scratch();
            let mut budget = Budget::new(4096);
            let arena = crate::mem::arena::Arena::new(&mut budget, "owned writes", 4096).unwrap();
            crate::mem::guard::forbid_alloc(|| {
                let mut images = arena.alloc_owned_slice_with(3, |_| None).unwrap();
                images[0] = Some(storage.heap.append_row_image(&[Datum::Int4(1)]).unwrap());
                images[1] = Some(storage.heap.append_row_image(&[Datum::Int4(2)]).unwrap());
                storage.compact_heap(&mut scratch).unwrap();
                images[2] = Some(storage.heap.append_row_image(&[Datum::Int4(3)]).unwrap());
                assert_eq!(
                    storage.compact_heap(&mut scratch).unwrap_err().sqlstate,
                    sqlstate::PROGRAM_LIMIT_EXCEEDED
                );
                let mut undos = [None; 3];
                for index in 0..3 {
                    let image = images[index].take();
                    undos[index] = Some(
                        if tracked {
                            storage.write_pending_image(table, index as u64 + 1, 7, 1, image)
                        } else {
                            storage.write_pending_image_untracked(
                                table,
                                index as u64 + 1,
                                7,
                                1,
                                image,
                            )
                        }
                        .unwrap(),
                    );
                }
                drop(images);
                for (index, undo) in undos.into_iter().enumerate() {
                    let rowid = index as u64 + 1;
                    let row = storage.resident_row_state(table, rowid).unwrap().unwrap();
                    let Some(Some(RowHome::Heap(location))) =
                        row.visible_at(7, SNAPSHOT_ALL, u64::MAX)
                    else {
                        panic!("owned input must be visible")
                    };
                    let bytes = storage.heap.get(location).unwrap();
                    let mut values = [Datum::Null; 1];
                    rowenc::decode(&bytes, &[ColType::Int4], &mut values).unwrap();
                    assert_eq!(values[0], Datum::Int4(rowid as i32));
                    drop(bytes);
                    drop(row);
                    storage.restore_pending(table, rowid, 7, undo.unwrap());
                }
                assert!(storage.tables[table].rows.is_empty());
                if !tracked {
                    assert!(
                        storage
                            .cumulative_statistics()
                            .relation_transactions
                            .is_empty()
                    );
                }
                let current = storage.heap.retain_allocation();
                storage.compact_heap(&mut scratch).unwrap();
                drop(current);
                assert_eq!(storage.heap.used(), 0);
            });
        }
    }

    #[test]
    fn owned_write_inputs_failed_publication_drops_unconsumed_rewrite_images() {
        let mut config = test_config();
        config.max_row_versions_per_row = 1;
        let (mut storage, table) = fixture(&config);
        let mut scratch = relocation_scratch();
        let mut budget = Budget::new(4096);
        let arena =
            crate::mem::arena::Arena::new(&mut budget, "failed owned writes", 4096).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mut undo = None;
            let result = (|| -> Result<(), SqlError> {
                let mut images = arena.alloc_owned_slice_with(3, |_| None).map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "test statement arena full"
                    )
                })?;
                images[0] = Some(storage.heap.append_row_image(&[Datum::Int4(1)])?);
                images[2] = Some(storage.heap.append_row_image(&[Datum::Int4(3)])?);
                storage.compact_heap(&mut scratch)?;
                images[1] = Some(storage.heap.append_row_image(&[Datum::Int4(2)])?);
                undo = Some(storage.write_pending_image_untracked(
                    table,
                    1,
                    7,
                    1,
                    images[0].take(),
                )?);
                storage.write_pending_image_untracked(table, 1, 7, 2, images[1].take())?;
                Ok(())
            })();
            assert_eq!(
                result.unwrap_err().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            let row = storage.resident_row_state(table, 1).unwrap().unwrap();
            assert_eq!(row.pending.len(), 1);
            drop(row);
            storage.restore_pending(table, 1, 7, undo.unwrap());
            assert!(storage.tables[table].rows.is_empty());
            // The third, unconsumed input belonged to the spare buffer.
            let current = storage.heap.retain_allocation();
            storage.compact_heap(&mut scratch).unwrap();
            drop(current);
            assert_eq!(storage.heap.used(), 0);
        });
    }

    #[test]
    fn pending_image_ownership_insert_survives_relocation_before_publication() {
        let (mut storage, table) = fixture(&test_config());
        let mut scratch = relocation_scratch();
        storage.heap.append_bytes(b"unpublished garbage").unwrap();
        let location = storage.heap.append_row(&[Datum::Int4(41)]).unwrap();
        let input = storage.heap.get(location).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let prepared = storage
                .prepare_pending_write_image(table, 1, 7, 1, Some(&input), true)
                .unwrap();
            storage.compact_heap(&mut scratch).unwrap();
            assert_eq!(storage.heap.used(), 0);
            assert!(!storage.heap.is_current_allocation(&input));
            assert!(matches!(
                storage.publish_pending_write(prepared).unwrap(),
                PendingPublication::Retry
            ));
            let undo = storage
                .write_pending_image_inner(table, 1, 7, 1, Some(input.clone()), true)
                .unwrap();
            let row = storage.resident_row_state(table, 1).unwrap().unwrap();
            let Some(Some(RowHome::Heap(current))) = row.visible_at(7, SNAPSHOT_ALL, u64::MAX)
            else {
                panic!("inserted image must be visible to its writer")
            };
            assert_ne!(current.generation, location.generation);
            assert_eq!(&*storage.heap.get(current).unwrap(), &*input);
            drop(row);
            storage.restore_pending(table, 1, 7, undo);
            assert!(storage.resident_row_state(table, 1).unwrap().is_none());
        });
    }

    #[test]
    fn pending_image_ownership_update_preserves_readers_and_rollback() {
        let (mut storage, table) = fixture(&test_config());
        let mut scratch = relocation_scratch();
        storage
            .heap
            .append_bytes(b"garbage before live row")
            .unwrap();
        let old = storage.heap.append_row(&[Datum::Int4(10)]).unwrap();
        storage.tables[table]
            .rows
            .insert(1, RowState::committed_only_at(old, 7))
            .unwrap();
        storage.set_lsn(7);
        let reader = storage.heap.get(old).unwrap();
        let location = storage.heap.append_row(&[Datum::Int4(29)]).unwrap();
        let input = storage.heap.get(location).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let prepared = storage
                .prepare_pending_write_image(table, 1, 8, 1, Some(&input), true)
                .unwrap();
            storage.compact_heap(&mut scratch).unwrap();
            assert!(matches!(
                storage.publish_pending_write(prepared).unwrap(),
                PendingPublication::Retry
            ));
            let undo = storage
                .write_pending_image_inner(table, 1, 8, 1, Some(input.clone()), true)
                .unwrap();
            let row = storage.resident_row_state(table, 1).unwrap().unwrap();
            let Some(Some(RowHome::Heap(updated))) = row.visible_at(8, SNAPSHOT_ALL, 7) else {
                panic!("writer must see its update")
            };
            let Some(Some(RowHome::Heap(committed))) = row.visible_at(9, SNAPSHOT_ALL, 7) else {
                panic!("other reader must see the committed row")
            };
            assert_eq!(&*storage.heap.get(updated).unwrap(), &*input);
            assert_eq!(&*storage.heap.get(committed).unwrap(), &*reader);
            drop(row);
            storage.restore_pending(table, 1, 8, undo);
            let row = storage.resident_row_state(table, 1).unwrap().unwrap();
            assert!(row.pending.is_none());
            assert_eq!(
                row.visible_at(8, SNAPSHOT_ALL, 7),
                Some(Some(RowHome::Heap(committed)))
            );
            assert_eq!(&*storage.heap.get(committed).unwrap(), &*reader);
        });
    }

    #[test]
    fn pending_image_ownership_capacity_error_releases_old_allocation() {
        let mut config = test_config();
        config.memtable_bytes = 128;
        let (mut storage, table) = fixture(&config);
        let mut scratch = relocation_scratch();
        let filler = [0u8; 128];
        let location = storage.heap.append_row(&[Datum::Int4(41)]).unwrap();
        let input = storage.heap.get(location).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let prepared = storage
                .prepare_pending_write_image(table, 1, 7, 1, Some(&input), true)
                .unwrap();
            storage.compact_heap(&mut scratch).unwrap();
            assert!(matches!(
                storage.publish_pending_write(prepared).unwrap(),
                PendingPublication::Retry
            ));
            storage.heap.append_bytes(&filler).unwrap();
            let error = storage
                .write_pending_image_inner(table, 1, 7, 1, Some(input), true)
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(storage.resident_row_state(table, 1).unwrap().is_none());
            assert!(
                storage
                    .cumulative_statistics()
                    .relation_transactions
                    .is_empty()
            );
            // Force a copy into the old allocation. A leaked input owner would
            // prevent this reclamation even though no row was published.
            let current = storage.heap.retain_allocation();
            storage.compact_heap(&mut scratch).unwrap();
            assert_eq!(storage.heap.used(), 0);
            drop(current);
            let versions = storage.row_versions.read();
            for slot in 0..versions.pending_row_versions.len() {
                assert!(!versions.pending_row_versions[slot].used);
            }
        });
    }

    #[test]
    fn pending_image_ownership_reused_target_rejects_before_relocation_retry() {
        let (mut storage, table) = fixture(&test_config());
        let mut scratch = relocation_scratch();
        let location = storage.heap.append_row(&[Datum::Int4(41)]).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let prepared = storage
                .prepare_pending_write(table, 1, 7, 1, Some(location), true)
                .unwrap();
            storage.compact_heap(&mut scratch).unwrap();
            storage.commit_drop(table);
            let replacement = storage
                .create_table(make_def("replacement", &[("id", ColType::Int4, false)]))
                .unwrap();
            assert_eq!(replacement, table);
            let error = storage.publish_pending_write(prepared).err().unwrap();
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            assert!(storage.tables[table].rows.is_empty());
            // Publication failure must release the last owner of the old buffer.
            let current = storage.heap.retain_allocation();
            storage.compact_heap(&mut scratch).unwrap();
            drop(current);
            assert_eq!(storage.heap.used(), 0);
        });
    }

    #[test]
    fn row_read_ownership_retained_metadata_allows_unrelated_publication() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};
        let config = test_config();
        let (mut storage, table) = fixture(&config);
        let original = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        storage.tables[table]
            .rows
            .insert(1, RowState::committed_only_at(original, 7))
            .unwrap();
        storage.set_lsn(7);
        let retained = storage.resident_row_state(table, 1).unwrap().unwrap();
        let location = storage.heap.append_row(&[Datum::Int4(20)]).unwrap();
        let finished = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                crate::mem::guard::forbid_alloc(|| {
                    let undo = storage
                        .write_pending(table, 2, 8, 1, Some(location))
                        .unwrap();
                    storage.restore_pending(table, 2, 8, undo);
                    finished.store(true, Ordering::Release);
                });
            });
            let deadline = Instant::now() + Duration::from_secs(2);
            while !finished.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::yield_now();
            }
            let progressed = finished.load(Ordering::Acquire);
            assert_eq!(retained.committed_lsn, 7);
            // Release before reporting a failure so a regression cannot hang CI.
            drop(retained);
            assert!(
                progressed,
                "retained metadata blocked an unrelated row publication"
            );
        });
    }

    #[test]
    fn shared_row_publication_completes_with_a_retained_visible_reader() {
        let (mut storage, table) = fixture(&test_config());
        let old = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        storage.tables[table]
            .rows
            .insert(1, RowState::committed_only_at(old, 7))
            .unwrap();
        let row = storage.resident_row_state(table, 1).unwrap().unwrap();
        let image = storage
            .visible_row_home_at(table, 1, row, 8, SNAPSHOT_ALL, 7)
            .unwrap()
            .unwrap();
        let completed = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let storage = &storage;
            let completed = &completed;
            let writer = scope.spawn(move || {
                crate::mem::guard::forbid_alloc(|| {
                    let next = storage.heap.append_row(&[Datum::Int4(2)]).unwrap();
                    let undo = storage.write_pending(table, 2, 7, 1, Some(next)).unwrap();
                    completed.store(true, Ordering::Release);
                    undo
                })
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while !completed.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::yield_now();
            }
            let progressed = completed.load(Ordering::Acquire);
            crate::mem::guard::forbid_alloc(|| {
                storage
                    .with_row_bytes(table, 1, image, |bytes| {
                        assert_eq!(bytes, &*storage.heap.get(old).unwrap());
                        Ok(())
                    })
                    .unwrap();
            });
            let undo = writer.join().unwrap();
            assert!(
                progressed,
                "publication must complete before the retained reader releases bytes"
            );
            crate::mem::guard::forbid_alloc(|| {
                assert_eq!(
                    storage
                        .resident_row_state(table, 2)
                        .unwrap()
                        .unwrap()
                        .pending_last()
                        .unwrap()
                        .txid,
                    7
                );
                storage.restore_pending(table, 2, 7, undo);
                assert!(storage.resident_row_state(table, 2).unwrap().is_none());
            });
        });
    }

    #[test]
    fn shared_row_publication_defers_definition_waits_without_reserving_other_tables() {
        let (mut storage, table) = fixture(&test_config());
        let other = storage
            .create_table(make_def(
                "unrelated_admission",
                &[("id", ColType::Int4, false)],
            ))
            .unwrap();
        let location = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        let prepared = storage
            .prepare_pending_write(table, 1, 7, 1, Some(location), true)
            .unwrap();
        let definition = storage.tables[table].definition.write();
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        std::thread::scope(|scope| {
            let storage = &storage;
            scope.spawn(move || {
                let result =
                    crate::mem::guard::forbid_alloc(|| storage.publish_pending_write(prepared));
                send.send(result).unwrap();
            });
            let result = receive.recv_timeout(Duration::from_secs(5));
            // Release before assertions so even the blocking regression can
            // finish its scope without stranding a worker.
            if matches!(result, Ok(Ok(PendingPublication::DefinitionBusy))) {
                crate::mem::guard::forbid_alloc(|| {
                    let undo = storage
                        .write_pending(other, 1, 8, 1, Some(location))
                        .unwrap();
                    storage.restore_pending(other, 1, 8, undo);
                });
            }
            drop(definition);
            assert!(
                matches!(result, Ok(Ok(PendingPublication::DefinitionBusy))),
                "a held definition must defer without retaining the global snapshot registry"
            );
        });
        crate::mem::guard::forbid_alloc(|| {
            let undo = storage
                .write_pending(table, 1, 7, 1, Some(location))
                .unwrap();
            storage.restore_pending(table, 1, 7, undo);
        });
    }

    #[test]
    fn shared_row_publication_rejects_retirement_before_and_after_slot_reuse() {
        for retirement in 0..3 {
            let (storage, table) = fixture(&test_config());
            let location = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
            crate::mem::guard::forbid_alloc(|| {
                let retired = storage
                    .prepare_pending_write(table, 1, 7, 1, Some(location), true)
                    .unwrap();
                let reused = storage
                    .prepare_pending_write(table, 1, 7, 1, Some(location), false)
                    .unwrap();
                match retirement {
                    0 => storage.commit_drop(table),
                    1 => storage.rollback_create(table),
                    _ => storage.drop_table(table),
                }
                let error = storage.publish_pending_write(retired).err().unwrap();
                assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
                assert!(storage.tables[table].rows.is_empty());
                for tracked in [true, false] {
                    let error = storage
                        .write_pending_inner(table, 1, 7, 1, Some(location), tracked)
                        .unwrap_err();
                    assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
                }
                let next = storage
                    .create_table_in(
                        make_def("replacement_publication", &[("id", ColType::Int4, false)]),
                        7,
                    )
                    .unwrap();
                assert_eq!(next, table);
                let error = storage.publish_pending_write(reused).err().unwrap();
                assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
                assert!(storage.tables[table].rows.is_empty());
                let undo = storage
                    .write_pending(table, 1, 7, 1, Some(location))
                    .unwrap();
                storage.restore_pending(table, 1, 7, undo);
                assert!(storage.tables[table].rows.is_empty());
                let versions = storage.row_versions.read();
                for slot in 0..versions.pending_row_versions.len() {
                    assert!(!versions.pending_row_versions[slot].used);
                }
            });
        }
    }

    #[test]
    fn shared_row_publication_rejects_database_replacement_with_the_same_catalog_stamp() {
        let (storage, table) = fixture(&test_config());
        let location = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let prepared = storage
                .prepare_pending_write(table, 1, 7, 1, Some(location), true)
                .unwrap();
            let stamp = storage.tables[table].created_at();
            {
                let mut definition = storage.tables[table].definition.write();
                // Database cloning preserves catalog stamps; the owner must
                // remain part of a prepared write's identity.
                definition.identity.database =
                    if definition.identity.database == DatabaseOid::POSTGRES {
                        DatabaseOid::TEMPLATE1
                    } else {
                        DatabaseOid::POSTGRES
                    };
            }
            assert_eq!(storage.tables[table].created_at(), stamp);
            let error = storage.publish_pending_write(prepared).err().unwrap();
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            assert!(storage.tables[table].rows.is_empty());
        });
    }

    #[test]
    fn shared_row_publication_admission_preserves_pending_create_and_drop_rollback() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let storage = Storage::new(&config, &mut budget).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let table = storage
                .create_table_in(
                    make_def("pending_admission", &[("id", ColType::Int4, false)]),
                    7,
                )
                .unwrap();
            let location = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
            let undo = storage
                .write_pending(table, 1, 7, 1, Some(location))
                .unwrap();
            storage.restore_pending(table, 1, 7, undo);
            storage.drop_table_in(table, 7);
            assert_eq!(
                storage
                    .write_pending(table, 1, 7, 1, Some(location))
                    .unwrap_err()
                    .sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            storage.rollback_drop(table);
            storage.commit_create(table);
            let prepared = storage
                .prepare_pending_write(table, 1, 7, 1, Some(location), true)
                .unwrap();
            storage.drop_table_in(table, 8);
            assert!(matches!(
                storage.publish_pending_write(prepared).unwrap(),
                PendingPublication::Wait(8, _)
            ));
            storage.rollback_drop(table);
            let undo = storage
                .write_pending(table, 1, 7, 1, Some(location))
                .unwrap();
            storage.restore_pending(table, 1, 7, undo);
            assert!(storage.tables[table].rows.is_empty());
        });
    }

    #[test]
    fn shared_row_publication_conflicting_preparations_cannot_both_publish() {
        let (storage, table) = fixture(&test_config());
        let location = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        let first = storage
            .prepare_pending_write(table, 1, 7, 1, Some(location), true)
            .unwrap();
        let second = storage
            .prepare_pending_write(table, 1, 8, 1, Some(location), true)
            .unwrap();
        let barrier = std::sync::Barrier::new(3);
        std::thread::scope(|scope| {
            let run = |prepared| {
                barrier.wait();
                crate::mem::guard::forbid_alloc(|| storage.publish_pending_write(prepared).unwrap())
            };
            let first = scope.spawn(move || run(first));
            let second = scope.spawn(move || run(second));
            barrier.wait();
            let results = [first.join().unwrap(), second.join().unwrap()];
            assert_eq!(
                results
                    .iter()
                    .filter(|result| matches!(result, PendingPublication::Written(_)))
                    .count(),
                1
            );
            let row = storage.resident_row_state(table, 1).unwrap().unwrap();
            let owner = row.pending_last().unwrap().txid;
            assert_eq!(row.pending.len(), 1);
            assert!(results.iter().any(
                |result| matches!(result, PendingPublication::Wait(blocker, _) if *blocker == owner)
            ));
            drop(row);
            let loser = if owner == 7 { 8 } else { 7 };
            assert_eq!(
                storage
                    .write_pending(table, 1, loser, 1, Some(location))
                    .unwrap_err()
                    .sqlstate,
                sqlstate::INTERNAL_LOCK_WAIT
            );
        });
    }

    #[test]
    fn shared_row_publication_revalidates_reused_pending_slots_and_undo() {
        let (storage, table) = fixture(&test_config());
        let location = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let prior = storage
                .write_pending(table, 1, 7, 1, Some(location))
                .unwrap();
            let stale = storage
                .prepare_pending_write(table, 1, 7, 2, None, true)
                .unwrap();
            storage.restore_pending(table, 1, 7, prior);
            let current = storage
                .write_pending(table, 1, 7, 1, Some(location))
                .unwrap();
            assert_eq!(stale.expected, storage.tables[table].rows.get(&1));
            assert!(matches!(
                storage.publish_pending_write(stale).unwrap(),
                PendingPublication::Retry
            ));
            storage.restore_pending(table, 1, 7, prior);
            assert_eq!(
                storage
                    .resident_row_state(table, 1)
                    .unwrap()
                    .unwrap()
                    .pending
                    .len(),
                1
            );
            let deletion = storage.write_pending(table, 1, 7, 2, None).unwrap();
            storage.restore_pending(table, 1, 7, current);
            assert_eq!(
                storage
                    .resident_row_state(table, 1)
                    .unwrap()
                    .unwrap()
                    .pending
                    .len(),
                2
            );
            storage.restore_pending(table, 1, 7, deletion);
            storage.restore_pending(table, 1, 7, current);
            assert!(storage.resident_row_state(table, 1).unwrap().is_none());
        });
    }

    #[test]
    fn shared_row_publication_parallel_writers_keep_images_and_statistics_coherent() {
        let mut config = test_config();
        config.max_connections = 4;
        let (storage, table) = fixture(&config);
        let barrier = std::sync::Barrier::new(5);
        std::thread::scope(|scope| {
            for worker in 0..4u32 {
                let storage = &storage;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    crate::mem::guard::forbid_alloc(|| {
                        for item in 1..=16 {
                            let rowid = u64::from(worker * 16 + item);
                            let location = storage
                                .heap
                                .append_row(&[Datum::Int4(rowid as i32)])
                                .unwrap();
                            storage
                                .write_pending(table, rowid, worker + 7, 1, Some(location))
                                .unwrap();
                        }
                    });
                });
            }
            barrier.wait();
        });
        crate::mem::guard::forbid_alloc(|| {
            for rowid in 1..=64 {
                let row = storage.resident_row_state(table, rowid).unwrap().unwrap();
                let pending = row.pending_last().unwrap();
                assert_eq!(pending.txid, ((rowid - 1) / 16) as u32 + 7);
                let bytes = storage.heap.get(pending.loc.unwrap()).unwrap();
                let mut values = [Datum::Null];
                rowenc::decode(&bytes, &[ColType::Int4], &mut values).unwrap();
                assert_eq!(values, [Datum::Int4(rowid as i32)]);
            }
            let statistics = storage.cumulative_statistics();
            assert_eq!(statistics.relation_transactions.len(), 4);
            assert!(
                statistics
                    .relation_transactions
                    .iter()
                    .all(|entry| entry.n_tup_ins == 16 && entry.n_tup_upd == 0)
            );
        });
    }

    #[test]
    fn shared_row_publication_statistics_failure_is_hidden_from_readers() {
        let (storage, table) = fixture(&test_config());
        let location = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        let prepared = storage
            .prepare_pending_write(table, 1, 7, 1, Some(location), true)
            .unwrap();
        let mut statistics = storage.cumulative_statistics();
        while statistics.relation_transactions.len() < statistics.relation_transactions.capacity() {
            statistics
                .relation_transactions
                .push(RelationTransactionStatistics::new(8, table, 1))
                .unwrap();
        }
        std::thread::scope(|scope| {
            let storage = &storage;
            let writer = scope.spawn(move || {
                crate::mem::guard::forbid_alloc(|| {
                    storage
                        .publish_pending_write(prepared)
                        .err()
                        .unwrap()
                        .sqlstate
                })
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut publishing = false;
            while Instant::now() < deadline {
                if storage.tables[table].rows.test_write().is_err() {
                    publishing = true;
                    break;
                }
                std::thread::yield_now();
            }
            let reader = scope.spawn(move || {
                crate::mem::guard::forbid_alloc(|| {
                    storage.resident_row_state(table, 1).unwrap().is_none()
                })
            });
            drop(statistics);
            assert_eq!(writer.join().unwrap(), sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(
                reader.join().unwrap(),
                "a failed pending head must never become visible"
            );
            assert!(
                publishing,
                "writer must reach the statistics check under publication ownership"
            );
        });
        assert!(storage.row_versions.test_write().is_ok());
        assert!(storage.tables[table].rows.test_write().is_ok());
    }

    #[test]
    fn shared_row_publication_rollback_preserves_committed_deletion_markers() {
        let (mut storage, table) = fixture(&test_config());
        let location = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        let marker = RowState {
            committed: None,
            committed_lsn: 9,
            committed_generation: Some(0),
            checkpoint_change_lsn: 9,
            history: CommittedHistory::empty(),
            pending: PendingVersions::empty(),
        };
        storage.tables[table].rows.insert(1, marker).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let undo = storage
                .write_pending(table, 1, 7, 1, Some(location))
                .unwrap();
            storage.restore_pending(table, 1, 7, undo);
            assert_eq!(storage.tables[table].rows.get(&1), Some(marker));
            let row = storage.resident_row_state(table, 1).unwrap().unwrap();
            assert_eq!(row.visible_at(8, SNAPSHOT_ALL, 9), Some(None));
            assert_eq!(row.checkpoint_change_lsn, 9);
        });
    }

    #[test]
    fn shared_row_publication_capacity_failures_preserve_reusable_ownership() {
        let mut config = test_config();
        config.table_rows = 1;
        config.txn_rows = 1;
        config.max_row_versions_per_row = 1;
        let (storage, table) = fixture(&config);
        let location = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let undo = storage
                .write_pending(table, 1, 7, 1, Some(location))
                .unwrap();
            for rowid in [1, 2] {
                assert_eq!(
                    storage
                        .write_pending(table, rowid, 7, 2, None)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::PROGRAM_LIMIT_EXCEEDED
                );
            }
            assert_eq!(
                storage
                    .resident_row_state(table, 1)
                    .unwrap()
                    .unwrap()
                    .pending
                    .len(),
                1
            );
            storage.restore_pending(table, 1, 7, undo);
            storage
                .write_pending(table, 2, 7, 2, Some(location))
                .unwrap();
            assert!(storage.row_versions.test_write().is_ok());
            assert!(storage.tables[table].rows.test_write().is_ok());
        });
    }

    #[test]
    fn shared_row_publication_pool_exhaustion_leaves_no_row_or_statistics() {
        let mut config = test_config();
        config.max_connections = 1;
        config.max_prepared_transactions = 0;
        config.txn_rows = 1;
        let capacity = pending_row_version_capacity(&config) as u64;
        config.table_rows = capacity as usize + 1;
        let (storage, table) = fixture(&config);
        let location = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let undo = storage
                .write_pending(table, 1, 7, 1, Some(location))
                .unwrap();
            for rowid in 2..=capacity {
                storage
                    .write_pending(table, rowid, 7, 1, Some(location))
                    .unwrap();
            }
            assert_eq!(
                storage
                    .write_pending(table, capacity + 1, 7, 1, Some(location))
                    .unwrap_err()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert!(
                storage
                    .resident_row_state(table, capacity + 1)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                storage.cumulative_statistics().relation_transactions[0].n_tup_ins,
                capacity
            );
            storage.restore_pending(table, 1, 7, undo);
            storage
                .write_pending(table, capacity + 1, 7, 1, Some(location))
                .unwrap();
            assert!(storage.resident_row_state(table, 1).unwrap().is_none());
        });
    }

    #[test]
    fn shared_row_publication_rejects_retired_table_preparations() {
        let (mut storage, table) = fixture(&test_config());
        let location = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        let stale = storage
            .prepare_pending_write(table, 1, 7, 1, Some(location), true)
            .unwrap();
        storage.drop_table(table);
        let replacement = storage
            .create_table(make_def("replacement", &[("id", ColType::Int4, false)]))
            .unwrap();
        assert_eq!(replacement, table);
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                storage.publish_pending_write(stale).err().unwrap().sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            assert!(storage.resident_row_state(table, 1).unwrap().is_none());
            assert!(
                storage
                    .cumulative_statistics()
                    .relation_transactions
                    .is_empty()
            );
        });
    }

    #[test]
    fn shared_row_publication_preserves_active_history_on_capacity_failure() {
        let mut config = test_config();
        config.max_row_versions_per_row = 1;
        let (mut storage, table) = fixture(&config);
        let old = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        let current = storage.heap.append_row(&[Datum::Int4(2)]).unwrap();
        let mut state = RowState::committed_only_at(current, 9);
        push_committed_version(
            &mut storage.row_versions.exclusive().committed_row_versions,
            &mut state.history,
            1,
            CommittedVersion {
                home: Some(RowHome::Heap(old)),
                lsn: 7,
            },
        )
        .unwrap();
        storage.tables[table].rows.insert(1, state).unwrap();
        storage.register_snapshot(8, 7).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                storage
                    .write_pending(table, 1, 7, 1, None)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            let row = storage.resident_row_state(table, 1).unwrap().unwrap();
            assert_eq!(
                row.visible_at(8, SNAPSHOT_ALL, 7),
                Some(Some(RowHome::Heap(old)))
            );
            assert!(row.pending.is_none());
        });
        storage.release_snapshot(8);
        crate::mem::guard::forbid_alloc(|| {
            storage.write_pending(table, 1, 7, 1, None).unwrap();
            assert!(
                storage
                    .resident_row_state(table, 1)
                    .unwrap()
                    .unwrap()
                    .history
                    .is_empty()
            );
        });
    }
}
