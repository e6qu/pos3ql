//! Cache cleanup uses current table ownership and preserves reader boundaries.

use super::row_ownership::{acquire, try_mutex};
use super::*;

impl Storage {
    /// Publication still owns storage exclusively until every saved-file list
    /// is installed; only then may unneeded deletion markers disappear.
    pub(crate) fn clear_tombstones(&mut self, slot: usize) {
        self.tables[slot].rows.write().retain(|_, state| {
            state.committed.is_some() || !state.history.is_empty() || state.pending.is_some()
        });
    }

    /// Evict only versions covered by installed roots, retaining later writes.
    pub fn evict_committed(&self) {
        for slot in 0..self.tables.len() {
            self.evict_committed_rows(slot, false);
        }
    }

    /// Temporary spill may evict only its captured table generation.
    pub(crate) fn evict_committed_table(&self, slot: usize) {
        self.evict_committed_rows(slot, true);
    }

    fn evict_committed_rows(&self, slot: usize, include_temporary: bool) {
        let table = &self.tables[slot];
        let (definition, publication, generations, mut rows) = acquire(|| {
            Ok((
                table.definition.try_read_for_batch()?,
                try_mutex(&table.publication, "table publication lock poisoned")?,
                table.spill_generations.try_read_for_batch()?,
                table.rows.try_write_for_batch()?,
            ))
        });
        if !definition.identity.live()
            || generations.is_empty()
            || (!include_temporary
                && definition.committed.persistence == RelationPersistence::Temporary)
        {
            return;
        }
        for (_, state) in rows.iter_mut() {
            if let Some(RowHome::Heap(location)) = state.committed
                && let Some(sst) =
                    generations.eviction_member(state.committed_lsn, publication.generation)
            {
                state.committed = Some(RowHome::Spilled {
                    len: location.len,
                    sst,
                    commit_lsn: state.committed_lsn,
                });
            }
        }
    }

    /// Reader admission and pruning select the same retained history boundary.
    pub(crate) fn release_table_histories(&self, slot: usize) {
        let table = &self.tables[slot];
        let (snapshots, _definition, mut versions, mut rows) = acquire(|| {
            Ok((
                try_mutex(&self.snapshots, "snapshot state lock poisoned")?,
                table.definition.try_read_for_batch()?,
                self.row_versions.try_write_for_batch()?,
                table.rows.try_write_for_batch()?,
            ))
        });
        let oldest = snapshots.oldest();
        for (_, state) in rows.iter_mut() {
            prune_committed_history(
                &mut versions.committed_row_versions,
                &mut state.history,
                oldest,
            );
        }
    }

    /// Installed roots reproduce the removed prefix; later history stays local.
    pub fn release_durable_histories(&self) {
        for table in self.tables.iter() {
            let (definition, generations, mut versions, mut rows) = acquire(|| {
                Ok((
                    table.definition.try_read_for_batch()?,
                    table.spill_generations.try_read_for_batch()?,
                    self.row_versions.try_write_for_batch()?,
                    table.rows.try_write_for_batch()?,
                ))
            });
            if !definition.identity.live()
                || definition.committed.persistence == RelationPersistence::Temporary
            {
                continue;
            }
            let Some(through_lsn) = generations.published_through() else {
                continue;
            };
            for (_, state) in rows.iter_mut() {
                release_committed_history_through(
                    &mut versions.committed_row_versions,
                    &mut state.history,
                    through_lsn,
                );
            }
        }
    }

    /// Remove only states exactly reproduced by the installed saved files.
    pub fn evict_redundant_entries(&self, slot: usize) {
        let table = &self.tables[slot];
        let (_definition, generations, mut rows) = acquire(|| {
            Ok((
                table.definition.try_read_for_batch()?,
                table.spill_generations.try_read_for_batch()?,
                table.rows.try_write_for_batch()?,
            ))
        });
        if !generations.is_empty() {
            Self::evict_redundant_rows(&mut rows);
        }
    }

    /// Half-full overlays shed only redundant entries, within their table owner.
    pub fn evict_entries(&self) {
        for table in self.tables.iter() {
            let (definition, generations, mut rows) = acquire(|| {
                Ok((
                    table.definition.try_read_for_batch()?,
                    table.spill_generations.try_read_for_batch()?,
                    table.rows.try_write_for_batch()?,
                ))
            });
            if !definition.identity.live()
                || generations.is_empty()
                || rows.len() * 100 < rows.capacity() * 50
            {
                continue;
            }
            Self::evict_redundant_rows(&mut rows);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::types::{ColType, Datum};
    use crate::storage::tests::{make_def, test_budget, test_config};

    fn fixture(persistence: RelationPersistence) -> (Storage, usize, usize) {
        let mut config = test_config();
        config.table_rows = 2;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let mut definition = make_def("cleanup_owned", &[("id", ColType::Int4, false)]);
        definition.persistence = persistence;
        let first = storage.create_table(definition).unwrap();
        let second = storage
            .create_table(make_def("cleanup_other", &[("id", ColType::Int4, false)]))
            .unwrap();
        let location = storage.heap.append_row(&[Datum::Int4(10)]).unwrap();
        storage.tables[first]
            .rows
            .insert(1, RowState::committed_only_at(location, 7))
            .unwrap();
        storage.set_lsn(7);
        (storage, first, second)
    }

    fn saved_handle(tag: u8) -> crate::store::SstHandle {
        crate::store::SstHandle {
            index: crate::store::BlockId([tag; 32]),
            filter: crate::store::BlockId([tag + 1; 32]),
            roster: crate::store::BlockId([tag + 2; 32]),
            format: crate::store::RowSstFormat::PackedV4,
        }
    }

    fn update(storage: &Storage, table: usize, rowid: u64, value: i32, txid: u32, lsn: u64) {
        let image = storage
            .heap
            .append_row_image(&[Datum::Int4(value)])
            .unwrap();
        storage
            .write_pending_image_untracked(table, rowid, txid, 1, Some(image))
            .unwrap();
        storage.commit_row(table, rowid, txid, lsn).unwrap();
        storage.observe_lsn(lsn);
    }

    fn selected(storage: &Storage, table: usize, snapshot: u64) -> VisibleRowHome {
        let state = storage.resident_row_state(table, 1).unwrap().unwrap();
        storage
            .visible_row_home_at(table, 1, state, 99, SNAPSHOT_ALL, snapshot)
            .unwrap()
            .unwrap()
    }

    fn assert_value(storage: &Storage, table: usize, selected: VisibleRowHome, expected: i32) {
        storage
            .with_row_bytes(table, 1, selected, |bytes| {
                let mut values = [Datum::Null];
                rowenc::decode(bytes, &[ColType::Int4], &mut values)?;
                assert_eq!(values[0], Datum::Int4(expected));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn row_lock_owners_shared_cleanup_preserves_retained_bytes_and_later_writes() {
        let (mut storage, first, second) = fixture(RelationPersistence::Permanent);
        let location = storage.heap.append_row(&[Datum::Int4(40)]).unwrap();
        storage.tables[first]
            .rows
            .insert(2, RowState::committed_only_at(location, 7))
            .unwrap();
        storage
            .append_spill_through(first, saved_handle(1), 7)
            .unwrap();
        let generation = storage.tables[first].generation();
        storage.clear_table_dirty_through(first, generation);
        crate::mem::guard::forbid_alloc(|| {
            storage.register_snapshot(99, 7).unwrap();
            update(&storage, first, 1, 20, 8, 9);
            update(&storage, first, 1, 30, 10, 11);
        });
        let old = selected(&storage, first, 7);
        let current = selected(&storage, first, u64::MAX);
        let definition = storage.tables[first].definition();
        std::thread::scope(|scope| {
            let shared = &storage;
            let cleanup = scope.spawn(move || {
                crate::mem::guard::forbid_alloc(|| {
                    shared.release_durable_histories();
                    shared.evict_committed();
                    shared.evict_entries();
                    shared.evict_redundant_entries(first);
                });
            });
            crate::mem::guard::forbid_alloc(|| {
                update(&storage, second, 1, 50, 12, 13);
                assert_value(&storage, first, old, 10);
                assert_value(&storage, first, current, 30);
            });
            cleanup.join().unwrap();
        });
        crate::mem::guard::forbid_alloc(|| {
            let row = storage.resident_row_state(first, 1).unwrap().unwrap();
            assert_eq!(row.history.len(), 1);
            assert!(matches!(row.committed, Some(RowHome::Heap(_))));
            drop(row);
            assert!(storage.tables[first].rows.get(&2).is_none());
            assert_value(&storage, first, selected(&storage, first, 9), 20);
            assert_value(&storage, first, selected(&storage, first, u64::MAX), 30);
            assert_eq!(
                storage.tables[second].rows.get(&1).unwrap().committed_lsn,
                13
            );
            storage.release_snapshot(99);
        });
        drop(definition);
    }

    #[test]
    fn row_lock_owners_local_history_rechecks_readers_and_temporary_generation() {
        for matching_generation in [true, false] {
            let (mut storage, table, _) = fixture(RelationPersistence::Temporary);
            crate::mem::guard::forbid_alloc(|| {
                storage.register_snapshot(99, 7).unwrap();
                update(&storage, table, 1, 20, 8, 9);
            });
            let old = selected(&storage, table, 7);
            let generation = storage.tables[table].generation();
            storage
                .install_temporary_spill(table, saved_handle(1), generation, false)
                .unwrap();
            crate::mem::guard::forbid_alloc(|| {
                let shared = &storage;
                shared.release_table_histories(table);
                assert_eq!(shared.tables[table].rows.get(&1).unwrap().history.len(), 1);
                shared.release_durable_histories();
                assert_eq!(shared.tables[table].rows.get(&1).unwrap().history.len(), 1);

                shared.evict_committed();
                assert!(matches!(
                    shared.tables[table].rows.get(&1).unwrap().committed,
                    Some(RowHome::Heap(_))
                ));
                if !matching_generation {
                    update(shared, table, 1, 30, 10, 11);
                    shared.release_table_histories(table);
                    assert_eq!(shared.tables[table].rows.get(&1).unwrap().history.len(), 2);
                }
                shared.evict_committed_table(table);
                let state = shared.tables[table].rows.get(&1).unwrap();
                if matching_generation {
                    assert!(matches!(
                        state.committed,
                        Some(RowHome::Spilled { commit_lsn: 9, .. })
                    ));
                } else {
                    assert!(matches!(state.committed, Some(RowHome::Heap(_))));
                    assert_value(shared, table, selected(shared, table, u64::MAX), 30);
                }
                shared.release_snapshot(99);
                shared.release_table_histories(table);
                assert!(
                    shared.tables[table]
                        .rows
                        .get(&1)
                        .unwrap()
                        .history
                        .is_empty()
                );
                assert_value(shared, table, old, 10);
            });
        }
    }

    #[test]
    fn row_lock_owners_tombstone_cleanup_keeps_live_pending_and_historical_states() {
        const DELETED: usize = 1536;
        let mut config = test_config();
        config.table_rows = 2048;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let table = storage
            .create_table(make_def("cleanup_deleted", &[("id", ColType::Int4, false)]))
            .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let location = storage.heap.append_row(&[Datum::Int4(10)]).unwrap();
            let mut deleted = RowState::committed_only_at(location, 7);
            deleted.committed = None;
            let mut retained = [deleted; 4];
            retained[0].committed = Some(RowHome::Heap(location));
            retained[1].committed = Some(RowHome::Spilled {
                len: location.len,
                sst: 0,
                commit_lsn: 7,
            });
            {
                let versions = storage.row_versions.exclusive();
                push_committed_version(
                    &mut versions.committed_row_versions,
                    &mut retained[2].history,
                    config.max_row_versions_per_row,
                    CommittedVersion {
                        home: Some(RowHome::Heap(location)),
                        lsn: 6,
                    },
                )
                .unwrap();
                push_pending_version(
                    &mut versions.pending_row_versions,
                    &mut retained[3].pending,
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
            for rowid in 0..DELETED {
                storage.tables[table]
                    .rows
                    .insert(rowid as u64, deleted)
                    .unwrap();
            }
            for (index, state) in retained.iter().enumerate() {
                storage.tables[table]
                    .rows
                    .insert((DELETED + index) as u64, *state)
                    .unwrap();
            }
            storage.clear_tombstones(table);
            assert_eq!(storage.tables[table].rows.len(), retained.len());
            for rowid in 0..DELETED {
                assert!(storage.tables[table].rows.get(&(rowid as u64)).is_none());
            }
            for (index, state) in retained.iter().enumerate() {
                assert_eq!(
                    storage.tables[table].rows.get(&((DELETED + index) as u64)),
                    Some(*state)
                );
            }
            storage.clear_tombstones(table);
            assert_eq!(storage.tables[table].rows.len(), retained.len());
        });
    }
}
