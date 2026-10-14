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
}

impl Storage {
    pub(crate) fn captured_row_scan_in<'arena>(
        &self,
        table_slot: usize,
        txid: u32,
        arena: &'arena Arena,
    ) -> Result<CapturedRowScan<'arena>, SqlError> {
        let command = self.read_snapshot();
        let commit = self.commit_snapshot();
        let (source, records) = {
            let table = &self.tables[table_slot];
            let definition = table.definition.read();
            let publication = table.publication();
            let generations = table.spill_generations();
            // Version ownership precedes the row map, matching pending writes.
            // These guards end before saved-file lookup or any visitor runs.
            let versions = self.row_versions.read();
            let rows = table.rows.read();
            let image = SpillGenerationImage::capture(
                &definition,
                publication.generation,
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
                if Self::redundant_spilled_row_state(&state) {
                    continue;
                }
                let selected =
                    RowRead::borrowed(state, &versions).visible_version_at(txid, command, commit);
                records[at] = source.capture_selection(rowid, selected)?;
                at += 1;
            }
            (source, records)
        };
        for record in records.iter_mut() {
            source.resolve_saved(record, commit)?;
        }
        records.sort_unstable_by_key(|record| record.rowid);
        Ok(CapturedRowScan {
            source,
            rows: records,
        })
    }
}

impl<'source_arena> CapturedRowScan<'source_arena> {
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
        let mut storage = Storage::new(&config, &mut budget).unwrap();
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
                let versions = storage.row_versions.read();
                let state = storage.tables[table].rows.get(&2).unwrap();
                RowRead::borrowed(state, &versions)
                    .visible_version_at(11, 2, 7)
                    .unwrap()
                    .0
            };
            let later = storage.captured_row_scan_in(table, 11, &arena).unwrap();
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
            ] {
                let mut at = 0;
                let done = scan
                    .for_each_overlay(&mut |rowid, home| {
                        assert_eq!(rowid, expected[at].0);
                        assert_eq!(home.snapshot().version, versions[at]);
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
        let mut storage = Storage::new(&config, &mut budget).unwrap();
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
    fn captured_resident_byte_scans_release_failed_capture_and_callbacks() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
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
            assert!(storage.heap.test_write().is_ok());
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
                storage.heap.test_write().is_err(),
                "nested scan still retains its heap"
            );
            drop(nested);
            assert!(storage.heap.test_write().is_ok());
            storage.set_commit_snapshot(old_commit);
        });
    }
}
