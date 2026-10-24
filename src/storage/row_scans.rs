//! Captured SQL byte scans own resident selections and stream original saved files.

use super::row_images::{CapturedRowRecord, TableRowSnapshot};
use super::spill_generations::SpillGenerationImage;
use super::{RowHome, RowRead, RowScanIds, Storage, VisibleRowHome};
use crate::mem::arena::Arena;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use core::ops::ControlFlow;

pub(crate) struct CapturedRowScan<'arena> {
    source: TableRowSnapshot<'arena>,
    rows: &'arena [CapturedRowRecord],
    generation: u64,
}

/// One current committed position for validation, independent of the
/// transaction's query snapshot. Earlier own changes remain visible.
#[derive(Clone, Copy)]
pub(crate) struct ValidationRead {
    commit_lsn: u64,
}

impl ValidationRead {
    pub(crate) fn scan_in<'arena>(
        self,
        storage: &Storage,
        table: usize,
        txid: u32,
        arena: &'arena Arena,
    ) -> Result<CapturedRowScan<'arena>, SqlError> {
        storage.capture_byte_scan(table, txid, super::SNAPSHOT_ALL, self.commit_lsn, arena)
    }

    pub(crate) fn for_each_row_state(
        self,
        storage: &Storage,
        table: usize,
        each: &mut super::RowReadVisitor<'_>,
    ) -> Result<(), SqlError> {
        let scan = storage.metadata_row_scan_at(table, self.commit_lsn)?;
        if scan.for_each_overlay(each)?.is_continue() {
            scan.for_each_spilled_state(each)?;
        }
        Ok(())
    }

    pub(crate) fn visible_row_home(
        self,
        storage: &Storage,
        table: usize,
        rowid: u64,
        state: RowRead<'_>,
        txid: u32,
    ) -> Result<Option<VisibleRowHome>, SqlError> {
        storage.visible_row_home_at(
            table,
            rowid,
            state,
            txid,
            super::SNAPSHOT_ALL,
            self.commit_lsn,
        )
    }
}

impl Storage {
    pub(crate) fn validation_read(&self) -> ValidationRead {
        ValidationRead {
            commit_lsn: self.lsn(),
        }
    }

    pub(crate) fn captured_row_scan_in<'arena>(
        &self,
        table_slot: usize,
        txid: u32,
        arena: &'arena Arena,
    ) -> Result<CapturedRowScan<'arena>, SqlError> {
        self.capture_byte_scan(
            table_slot,
            txid,
            self.read_snapshot(),
            self.commit_snapshot(),
            arena,
        )
    }

    /// Index validation includes all own writes and current committed rows,
    /// even when the creating transaction has an older read snapshot.
    pub(crate) fn index_validation_scan_in<'arena>(
        &self,
        table_slot: usize,
        txid: u32,
        arena: &'arena Arena,
    ) -> Result<CapturedRowScan<'arena>, SqlError> {
        self.capture_byte_scan(table_slot, txid, super::SNAPSHOT_ALL, u64::MAX, arena)
    }

    fn capture_byte_scan<'arena>(
        &self,
        table_slot: usize,
        txid: u32,
        command: u32,
        commit: u64,
        arena: &'arena Arena,
    ) -> Result<CapturedRowScan<'arena>, SqlError> {
        crate::sql::query::check_timeout()?;
        let (source, records, generation) = {
            let table = &self.tables[table_slot];
            // A contended table releases every earlier owner before waiting.
            // No byte lookup or visitor runs during acquisition.
            let (definition, publication, generations, versions, rows) =
                super::row_ownership::acquire(|| {
                    Ok((
                        table.definition.try_read_for_batch()?,
                        super::row_ownership::try_mutex(
                            &table.publication,
                            "table publication lock poisoned",
                        )?,
                        table.spill_generations.try_read_for_batch()?,
                        self.row_versions.try_read_for_batch()?,
                        table.rows.try_read_for_batch()?,
                    ))
                });
            let image = SpillGenerationImage::capture(
                &definition,
                publication.generation,
                publication.row_layout,
                table_slot,
                commit,
            );
            let heap = rows.iter().any(|(_, state)| {
                matches!(state.committed, Some(RowHome::Heap(_)))
                    || !state.pending.is_none()
                    || !state.history.is_empty()
            });
            let source = TableRowSnapshot::capture(self, image, &generations, heap, Some(arena))?;
            let count = rows
                .iter()
                .filter(|(_, state)| !Self::redundant_spilled_row_state(state))
                .count();
            let records = arena
                .alloc_slice_with(count, |_| CapturedRowRecord::EMPTY)
                .map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "captured resident rows exceed the statement arena"
                    )
                })?;
            let mut at = 0;
            for (&rowid, &state) in rows.iter() {
                crate::sql::query::check_timeout()?;
                if Self::redundant_spilled_row_state(&state) {
                    continue;
                }
                let selected = RowRead::borrowed(
                    publication.row_layout.visible_state(state),
                    &versions,
                    publication.generation,
                    publication.row_layout,
                )
                .visible_version_at(txid, command, commit);
                records[at] = source.capture_selection(rowid, selected)?;
                at += 1;
            }
            (source, records, publication.generation)
        };
        for record in records.iter_mut() {
            crate::sql::query::check_timeout()?;
            source.resolve_saved(record, commit)?;
        }
        records.sort_unstable_by_key(|record| record.rowid);
        Ok(CapturedRowScan {
            source,
            rows: records,
            generation,
        })
    }
}

impl<'source_arena> CapturedRowScan<'source_arena> {
    pub(crate) fn read_snapshot(
        &self,
        rowid: u64,
        snapshot: super::RowSnapshot,
    ) -> Result<VisibleRowHome, SqlError> {
        let Ok(index) = self.rows.binary_search_by_key(&rowid, |row| row.rowid) else {
            return self.source.read_saved_snapshot(rowid, snapshot);
        };
        let home = self.source.read_record(self.rows[index])?.ok_or_else(|| {
            sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "captured row is invisible in its original source"
            )
        })?;
        if home.snapshot().created_at != snapshot.created_at
            || home.snapshot().version != snapshot.version
        {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "captured row has a different table or version"
            ));
        }
        Ok(home)
    }

    pub(super) fn publication_generation(
        &self,
        storage: &Storage,
        table: usize,
    ) -> Result<u64, SqlError> {
        if !self
            .source
            .matches_table(table, storage.tables[table].created_at())
        {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "statistics source belongs to a different table incarnation"
            ));
        }
        Ok(self.generation)
    }

    pub(crate) fn retain<'source>(
        &'source self,
        rowid: u64,
        home: &VisibleRowHome,
    ) -> Result<super::OrderedRowSnapshot<'source, 'source_arena>, SqlError> {
        self.source.retain(rowid, home)
    }

    pub(crate) fn for_each_home(
        &self,
        each: &mut dyn FnMut(u64, VisibleRowHome) -> Result<ControlFlow<()>, SqlError>,
    ) -> Result<ControlFlow<()>, SqlError> {
        crate::sql::query::check_timeout()?;
        if self.for_each_overlay(each)?.is_break() {
            return Ok(ControlFlow::Break(()));
        }
        self.source
            .for_each_saved_home(&RowScanIds::Captured(self.rows), each)
    }

    pub(crate) fn for_each_overlay(
        &self,
        each: &mut dyn FnMut(u64, VisibleRowHome) -> Result<ControlFlow<()>, SqlError>,
    ) -> Result<ControlFlow<()>, SqlError> {
        for &record in self.rows {
            crate::sql::query::check_timeout()?;
            if let Some(home) = self.source.read_record(record)?
                && each(record.rowid, home)?.is_break()
            {
                return Ok(ControlFlow::Break(()));
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    pub(crate) fn for_each_spilled_row_batch<'arena, 'callback>(
        &self,
        arena: &'arena Arena,
        recycle_rows: bool,
        decoded_columns: Option<u64>,
        each: &mut super::SpilledRowBatchVisitor<'arena, 'callback>,
    ) -> Result<(), SqlError> {
        Storage::for_each_spilled_row_batch(
            self.source.roots(),
            &RowScanIds::Captured(self.rows),
            arena,
            recycle_rows,
            decoded_columns,
            each,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;
    use crate::mem::fixed_vec::FixedVec;
    use crate::storage::row_images::{RowVersionIdentity, VisibleRowBytes};
    use crate::storage::tests::{make_def, test_budget, test_config};
    use crate::storage::{ColType, CommittedVersion, RowState};

    #[test]
    fn captured_resident_byte_scans_keep_unread_versions_after_chain_and_table_reuse() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CapturedRowScan<'static>>();
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let table = storage
            .create_table(make_def("captured_old", &[("id", ColType::Int4, true)]))
            .unwrap();
        let mut arena_budget = Budget::new(16 << 10);
        let arena = Arena::new(&mut arena_budget, "captured resident scans", 16 << 10).unwrap();
        let mut scratch = FixedVec::new(&mut budget, "captured relocation", 8).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let old_command = storage.read_snapshot();
            let old_commit = storage.commit_snapshot();
            let history = storage.heap.append_bytes(b"hist").unwrap();
            let current = storage.heap.append_bytes(b"keep").unwrap();
            let pending = storage.heap.append_bytes(b"pend").unwrap();
            let mut first = RowState::committed_only_at(current, 9);
            crate::storage::row_versions::push_committed_version(
                &mut storage.row_versions.exclusive().committed_row_versions,
                &mut first.history,
                config.max_row_versions_per_row,
                CommittedVersion {
                    lsn: 7,
                    home: Some(RowHome::Heap(history)),
                },
            )
            .unwrap();
            storage.tables[table].rows.insert(1, first).unwrap();
            storage.tables[table]
                .rows
                .insert(2, RowState::committed_only_at(current, 7))
                .unwrap();
            storage.tables[table]
                .rows
                .insert(3, RowState::committed_only_at(current, 7))
                .unwrap();
            storage
                .write_pending_untracked(table, 2, 11, 1, Some(pending))
                .unwrap();
            storage
                .write_pending_untracked(table, 3, 11, 1, None)
                .unwrap();
            storage.set_commit_snapshot(7);
            storage.set_read_snapshot(1);
            let earlier = storage.captured_row_scan_in(table, 11, &arena).unwrap();
            storage.set_read_snapshot(2);
            let pending_version = {
                let (generation, row_layout) = {
                    let publication = storage.table(table).publication();
                    (publication.generation, publication.row_layout)
                };
                let versions = storage.row_versions.read();
                let state = storage.tables[table].rows.get(&2).unwrap();
                RowRead::borrowed(state, &versions, generation, row_layout)
                    .visible_version_at(11, 2, 7)
                    .unwrap()
                    .0
            };
            let later = storage.captured_row_scan_in(table, 11, &arena).unwrap();
            let index = storage.index_validation_scan_in(table, 11, &arena).unwrap();
            assert!(storage.row_versions.test_write().is_ok());
            assert!(storage.tables[table].rows.test_write().is_ok());
            storage.commit_drop(table);
            let replacement = storage
                .create_table(make_def("captured_new", &[("id", ColType::Text, true)]))
                .unwrap();
            assert_eq!(replacement, table);
            let location = storage.heap.append_bytes(b"next").unwrap();
            storage
                .write_pending_untracked(table, 1, 12, 1, Some(location))
                .unwrap();
            storage.compact_heap(&mut scratch).unwrap();
            storage.set_read_snapshot(old_command);
            storage.set_commit_snapshot(old_commit);
            drop(storage);
            for (scan, expected, versions) in [
                (
                    &earlier,
                    &[(1, b"hist"), (2, b"keep"), (3, b"keep")][..],
                    &[RowVersionIdentity::Committed(7); 3][..],
                ),
                (
                    &later,
                    &[(1, b"hist"), (2, b"pend")][..],
                    &[RowVersionIdentity::Committed(7), pending_version][..],
                ),
                (
                    &index,
                    &[(1, b"keep"), (2, b"pend")][..],
                    &[RowVersionIdentity::Committed(9), pending_version][..],
                ),
            ] {
                let mut at = 0;
                let done = scan
                    .for_each_overlay(&mut |rowid, home| {
                        assert_eq!(rowid, expected[at].0);
                        assert_eq!(home.snapshot().version, versions[at]);
                        let snapshot = home.snapshot();
                        let reread = scan.read_snapshot(rowid, snapshot).unwrap();
                        let VisibleRowBytes::Heap(original) = reread.bytes else {
                            panic!("retained rewrite source is heap-backed")
                        };
                        assert_eq!(&*original, expected[at].1);
                        let mut replaced = snapshot;
                        replaced.created_at = replaced.created_at.wrapping_add(1);
                        assert_eq!(
                            scan.read_snapshot(rowid, replaced).err().unwrap().sqlstate,
                            sqlstate::SERIALIZATION_FAILURE
                        );
                        let VisibleRowBytes::Heap(bytes) = home.bytes else {
                            panic!("heap fixture")
                        };
                        assert_eq!(&*bytes, expected[at].1);
                        at += 1;
                        Ok(ControlFlow::Continue(()))
                    })
                    .unwrap();
                assert!(done.is_continue());
                assert_eq!(at, expected.len());
            }
        });
    }

    #[test]
    fn captured_resident_byte_scans_allow_version_release_while_a_callback_waits() {
        use crate::sql::types::Datum;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let table = storage
            .create_table(make_def(
                "captured_progress",
                &[("id", ColType::Int4, true)],
            ))
            .unwrap();
        let mut arena_budget = Budget::new(16 << 10);
        let arena = Arena::new(&mut arena_budget, "captured progress", 16 << 10).unwrap();
        let first = storage.heap.append_row(&[Datum::Int4(0)]).unwrap();
        let old = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        let pending = storage.heap.append_row(&[Datum::Int4(2)]).unwrap();
        storage.tables[table]
            .rows
            .insert(1, RowState::committed_only_at(first, 7))
            .unwrap();
        storage.tables[table]
            .rows
            .insert(2, RowState::committed_only_at(old, 7))
            .unwrap();
        let undo = storage
            .write_pending(table, 2, 11, 1, Some(pending))
            .unwrap();
        let old_command = storage.read_snapshot();
        let old_commit = storage.commit_snapshot();
        storage.set_read_snapshot(2);
        storage.set_commit_snapshot(7);
        let scan = crate::mem::guard::forbid_alloc(|| {
            storage.captured_row_scan_in(table, 11, &arena).unwrap()
        });
        let start = AtomicBool::new(false);
        let completed = AtomicBool::new(false);
        let mut progressed = false;
        std::thread::scope(|scope| {
            let storage = &storage;
            let start = &start;
            let completed = &completed;
            let writer = scope.spawn(move || {
                while !start.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                crate::mem::guard::forbid_alloc(|| {
                    storage.restore_pending(table, 2, 11, undo);
                    let replacement = storage.heap.append_row(&[Datum::Int4(3)]).unwrap();
                    let next = storage
                        .write_pending(table, 2, 12, 1, Some(replacement))
                        .unwrap();
                    completed.store(true, Ordering::Release);
                    next
                })
            });
            crate::mem::guard::forbid_alloc(|| {
                let mut count = 0;
                let done = scan
                    .for_each_overlay(&mut |rowid, home| {
                        if rowid == 1 {
                            start.store(true, Ordering::Release);
                            let deadline = Instant::now() + Duration::from_secs(5);
                            while !completed.load(Ordering::Acquire) && Instant::now() < deadline {
                                std::thread::yield_now();
                            }
                            progressed = completed.load(Ordering::Acquire);
                        }
                        let VisibleRowBytes::Heap(bytes) = home.bytes else {
                            panic!("heap fixture")
                        };
                        let mut values = [Datum::Null];
                        super::super::rowenc::decode(&bytes, &[ColType::Int4], &mut values)?;
                        assert_eq!(values[0], Datum::Int4(if rowid == 1 { 0 } else { 2 }));
                        count += 1;
                        Ok(ControlFlow::Continue(()))
                    })
                    .unwrap();
                assert!(done.is_continue());
                assert_eq!(count, 2);
            });
            let undo = writer.join().unwrap();
            assert!(
                progressed,
                "writer must release and reuse the unread version while the callback waits"
            );
            storage.restore_pending(table, 2, 12, undo);
        });
        storage.set_read_snapshot(old_command);
        storage.set_commit_snapshot(old_commit);
    }

    #[test]
    fn analyze_snapshot_nesting_and_failed_capture_restore_visibility_and_owners() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let table = storage
            .create_table(make_def("analyze_snapshot", &[("id", ColType::Int4, true)]))
            .unwrap();
        let mut small_budget = Budget::new(4);
        let small = Arena::new(&mut small_budget, "analysis capture capacity", 4).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let old_command = storage.read_snapshot();
            let old_commit = storage.commit_snapshot();
            storage.set_read_snapshot(1);
            storage.set_commit_snapshot(7);
            storage.set_lsn(11);
            let location = storage
                .heap
                .append_row(&[crate::sql::types::Datum::Int4(1)])
                .unwrap();
            storage.tables[table]
                .rows
                .insert(1, RowState::committed_only_at(location, 11))
                .unwrap();
            let before = storage.table_statistics(table, 0);
            let failed = storage.analyze_table(table, 0, &[], &small).unwrap_err();
            assert_eq!(failed.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(storage.read_snapshot(), 1);
            assert_eq!(storage.commit_snapshot(), 7);
            assert_eq!(storage.table_statistics(table, 0).valid, before.valid);
            assert!(storage.heap.test_exclusive_write().is_ok());
            let outer = storage.analyze_snapshot();
            assert_eq!(storage.read_snapshot(), super::super::SNAPSHOT_ALL);
            assert_eq!(storage.commit_snapshot(), 11);
            storage.set_lsn(13);
            let nested = storage.analyze_snapshot();
            assert_eq!(storage.commit_snapshot(), 11);
            drop(nested);
            assert_eq!(storage.commit_snapshot(), 11);
            drop(outer);
            assert_eq!(storage.read_snapshot(), 1);
            assert_eq!(storage.commit_snapshot(), 7);
            let retry = storage.analyze_snapshot();
            assert_eq!(storage.commit_snapshot(), 13);
            drop(retry);
            storage.set_read_snapshot(old_command);
            storage.set_commit_snapshot(old_commit);
        });
    }

    #[test]
    fn captured_resident_byte_scans_timeout_releases_capture_and_allows_retry() {
        struct ClearDeadline;
        impl Drop for ClearDeadline {
            fn drop(&mut self) {
                crate::sql::query::disarm_timeout();
            }
        }
        let _clear = ClearDeadline;
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let table = storage
            .create_table(make_def(
                "captured_deadline",
                &[("id", ColType::Int4, true)],
            ))
            .unwrap();
        let mut arena_budget = Budget::new(16 << 10);
        let arena = Arena::new(&mut arena_budget, "captured deadline", 16 << 10).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let old_commit = storage.commit_snapshot();
            storage.set_commit_snapshot(7);
            let location = storage.heap.append_bytes(b"keep").unwrap();
            storage.tables[table]
                .rows
                .insert(1, RowState::committed_only_at(location, 7))
                .unwrap();
            storage.tables[table]
                .rows
                .insert(2, RowState::committed_only_at(location, 7))
                .unwrap();
            crate::sql::query::arm_timeout(1);
            std::thread::sleep(std::time::Duration::from_millis(2));
            let error = storage
                .captured_row_scan_in(table, 0, &arena)
                .err()
                .unwrap();
            crate::sql::query::disarm_timeout();
            assert_eq!(error.sqlstate, sqlstate::QUERY_CANCELED);
            assert_eq!(arena.used(), 0);
            assert!(storage.heap.test_exclusive_write().is_ok());
            let scan = storage.captured_row_scan_in(table, 0, &arena).unwrap();
            let mut count = 0;
            let error = scan
                .for_each_home(&mut |_, _| {
                    count += 1;
                    crate::sql::query::arm_timeout(1);
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    Ok(ControlFlow::Continue(()))
                })
                .unwrap_err();
            crate::sql::query::disarm_timeout();
            assert_eq!(error.sqlstate, sqlstate::QUERY_CANCELED);
            assert_eq!(count, 1);
            assert!(storage.row_versions.test_write().is_ok());
            assert!(storage.tables[table].rows.test_write().is_ok());
            drop(scan);
            assert!(storage.heap.test_exclusive_write().is_ok());
            let retry = storage.captured_row_scan_in(table, 0, &arena).unwrap();
            let mut count = 0;
            assert!(
                retry
                    .for_each_home(&mut |_, _| {
                        count += 1;
                        Ok(ControlFlow::Continue(()))
                    })
                    .unwrap()
                    .is_continue()
            );
            assert_eq!(count, 2);
            drop(retry);
            assert!(storage.heap.test_exclusive_write().is_ok());
            storage.set_commit_snapshot(old_commit);
        });
    }

    #[test]
    fn captured_resident_byte_scans_release_failed_capture_and_callbacks() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let table = storage
            .create_table(make_def(
                "captured_capacity",
                &[("id", ColType::Int4, true)],
            ))
            .unwrap();
        let mut arena_budget = Budget::new(16 << 10);
        let arena = Arena::new(&mut arena_budget, "captured capacity", 16 << 10).unwrap();
        let mut small_budget = Budget::new(4);
        let small = Arena::new(&mut small_budget, "captured too small", 4).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let old_commit = storage.commit_snapshot();
            storage.set_commit_snapshot(7);
            let location = storage.heap.append_bytes(b"keep").unwrap();
            storage.tables[table]
                .rows
                .insert(1, RowState::committed_only_at(location, 7))
                .unwrap();
            storage.tables[table]
                .rows
                .insert(2, RowState::committed_only_at(location, 7))
                .unwrap();
            assert_eq!(
                storage
                    .captured_row_scan_in(table, 0, &small)
                    .err()
                    .unwrap()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert!(storage.heap.test_exclusive_write().is_ok());
            let scan = storage.captured_row_scan_in(table, 0, &arena).unwrap();
            let nested = storage.captured_row_scan_in(table, 0, &arena).unwrap();
            let mut count = 0;
            assert!(
                scan.for_each_overlay(&mut |_, _| {
                    assert!(storage.tables[table].rows.test_write().is_ok());
                    assert!(storage.row_versions.test_write().is_ok());
                    count += 1;
                    assert!(
                        nested
                            .for_each_overlay(&mut |_, _| Ok(ControlFlow::Break(())))?
                            .is_break()
                    );
                    Ok(ControlFlow::Break(()))
                })
                .unwrap()
                .is_break()
            );
            assert_eq!(count, 1);
            assert_eq!(
                scan.for_each_overlay(&mut |_, _| Err(sql_err!(
                    sqlstate::QUERY_CANCELED,
                    "cancel captured resident scan"
                )))
                .unwrap_err()
                .sqlstate,
                sqlstate::QUERY_CANCELED
            );
            drop(scan);
            assert!(
                storage.heap.test_exclusive_write().is_err(),
                "nested scan still retains its heap"
            );
            drop(nested);
            assert!(storage.heap.test_exclusive_write().is_ok());
            storage.set_commit_snapshot(old_commit);
        });
    }
}
