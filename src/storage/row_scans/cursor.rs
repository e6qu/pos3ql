//! A row-at-a-time scan retains selected versions and unfinished downloads.

use super::*;
use crate::config::Config;
use crate::mem::arena::{ArenaSliceError, ArenaSliceHandle};
use crate::mem::budget::{Budget, BudgetError};
use crate::storage::{
    MemberCursor, RowReadBuffers, SpillWalkProgress, SpilledRow, SpilledRowRepresentation,
};

pub(in crate::storage) struct RowCursor {
    source: Option<RowScanSource>,
}

struct RowScanSource {
    snapshot: TableRowSnapshot<'static>,
    rows: ArenaSliceHandle<CapturedRowRecord>,
    members: ArenaSliceHandle<MemberCursor>,
    resident: usize,
    commit: u64,
    done: bool,
    spill: SpillWalkProgress,
}

pub(in crate::storage) struct RowCursorBuffers {
    scan: crate::mem::mutex::Mutex<crate::storage::ScanContext>,
    row: RowReadBuffers,
}

impl RowCursorBuffers {
    pub(in crate::storage) fn allocation_bytes(config: &Config) -> usize {
        crate::storage::ScanContext::budget_bytes(config.max_spill_generations_per_table)
            - core::mem::size_of::<crate::mem::mutex::Mutex<crate::storage::ScanContext>>()
            + RowReadBuffers::budget_bytes()
            - core::mem::size_of::<RowReadBuffers>()
    }

    pub(in crate::storage) fn new(
        config: &Config,
        budget: &mut Budget,
    ) -> Result<Self, BudgetError> {
        budget.draw(Self::allocation_bytes(config), "query cursor byte buffers")?;
        Ok(Self {
            scan: crate::mem::mutex::Mutex::new(crate::storage::ScanContext::new(
                config.max_spill_generations_per_table,
            )),
            row: RowReadBuffers::new(),
        })
    }
}

fn state_error(error: ArenaSliceError) -> SqlError {
    match error {
        ArenaSliceError::Stale => sql_err!(
            sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
            "query cursor state arena was reset or replaced"
        ),
        ArenaSliceError::Borrowed => sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "query cursor state is already in use"
        ),
    }
}

fn cursor_capacity() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "query cursor positions exceed the execution arena"
    )
}

impl RowCursor {
    pub(in crate::storage) fn position_bytes(rows: usize, members: usize) -> Option<usize> {
        Arena::retained_slice_capacity::<CapturedRowRecord>(rows)?
            .checked_add(Arena::retained_slice_capacity::<MemberCursor>(members)?)
    }

    pub(in crate::storage) const fn closed() -> Self {
        Self { source: None }
    }

    pub(in crate::storage) fn close(&mut self) {
        self.source = None;
    }

    /// Capturing row identities and owners performs no object-store reads.
    pub(in crate::storage) fn open(
        &mut self,
        storage: &Storage,
        table: usize,
        txid: u32,
        arena: &Arena,
    ) -> Result<(), SqlError> {
        self.close();
        let commit = storage.commit_snapshot();
        let mut rows_handle = None;
        let (snapshot, rows, _) = storage.capture_byte_partition(
            table,
            txid,
            storage.read_snapshot(),
            commit,
            None,
            |count| {
                let handle = arena
                    .retain_slice_with(count, |_| CapturedRowRecord::EMPTY)
                    .map_err(|_| cursor_capacity())?;
                rows_handle = Some(handle);
                arena.retained_slice_mut(handle).map_err(state_error)
            },
        )?;
        drop(rows);
        let member_count = snapshot
            .roots()
            .with_view(|source| Ok(source.relation().len()))?
            .unwrap_or(0);
        let members = arena
            .retain_slice_with(member_count, |_| MemberCursor::EMPTY)
            .map_err(|_| cursor_capacity())?;
        self.source = Some(RowScanSource {
            snapshot,
            rows: rows_handle.expect("successful capture allocated its resident selections"),
            members,
            resident: 0,
            commit,
            done: false,
            spill: SpillWalkProgress::START,
        });
        Ok(())
    }

    pub(in crate::storage) fn rewind(&mut self, arena: &Arena) -> Result<(), SqlError> {
        let source = self.source.as_mut().ok_or_else(|| {
            sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "query row cursor is closed"
            )
        })?;
        arena
            .retained_slice_mut(source.members)
            .map_err(state_error)?
            .fill(MemberCursor::EMPTY);
        source.resident = 0;
        source.done = false;
        source.spill = SpillWalkProgress::START;
        Ok(())
    }

    pub(in crate::storage) fn next<'arena>(
        &mut self,
        buffers: &mut RowCursorBuffers,
        state_arena: &Arena,
        arena: &'arena Arena,
        decoded_columns: Option<&[bool; crate::storage::MAX_COLUMNS]>,
    ) -> Result<Option<SpilledRow<'arena>>, SqlError> {
        crate::sql::query::check_timeout()?;
        let source = self.source.as_mut().ok_or_else(|| {
            sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "query row cursor is closed"
            )
        })?;
        let mut rows = state_arena
            .retained_slice_mut(source.rows)
            .map_err(state_error)?;
        if source.done {
            return Ok(None);
        }
        while source.resident < rows.len() {
            let record = &mut rows[source.resident];
            source.snapshot.resolve_saved(record, source.commit)?;
            let Some(home) = source.snapshot.read_record(*record)? else {
                source.resident += 1;
                continue;
            };
            let copy = |bytes: &[u8]| {
                arena
                    .alloc_slice_copy(bytes)
                    .map(|bytes| &*bytes)
                    .map_err(|_| {
                        sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "query cursor row exceeds the statement arena"
                        )
                    })
            };
            let bytes = match home.bytes {
                crate::storage::row_images::VisibleRowBytes::Heap(bytes) => copy(&bytes)?,
                crate::storage::row_images::VisibleRowBytes::Spilled(row) => {
                    row.with_bytes_in(&mut buffers.row, copy)?
                }
            };
            let rowid = record.rowid;
            source.resident += 1;
            return Ok(Some(SpilledRow {
                rowid,
                representation: SpilledRowRepresentation::Encoded(bytes),
            }));
        }
        let mut members = state_arena
            .retained_slice_mut(source.members)
            .map_err(state_error)?;
        let mut row = None;
        let complete = Storage::spill_merged_walk_bytes(
            source.snapshot.roots(),
            arena,
            false,
            decoded_columns,
            true,
            crate::storage::SpillOverlayMode::VisibleScan(&RowScanIds::Captured(&rows)),
            Some(
                source
                    .spill
                    .resume(&mut members, &buffers.scan, 1, u64::MAX),
            ),
            &mut |rowid, _, representation| {
                row = Some(SpilledRow {
                    rowid,
                    representation,
                });
                Ok(ControlFlow::Break(()))
            },
        )?;
        source.done = complete;
        if row.is_none() && !complete {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "query row cursor stopped without a row or storage wait"
            ));
        }
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::types::{ColType, Datum};
    use crate::storage::RowState;
    use crate::storage::tests::{make_def, test_budget, test_config};

    #[test]
    fn query_row_cursor_retains_unread_heap_rows_without_borrowing_storage() {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let _visibility = storage.nested_statement_snapshot(7);
        let table = storage
            .create_table(make_def("row_cursor", &[("n", ColType::Int4, true)]))
            .unwrap();
        for rowid in 1..=3 {
            let location = storage
                .heap
                .append_row(&[Datum::Int4(rowid as i32)])
                .unwrap();
            storage.tables[table]
                .rows
                .insert(rowid, RowState::committed_only_at(location, 7))
                .unwrap();
        }
        let mut read_budget = Budget::new(RowCursorBuffers::allocation_bytes(&config) + 8192);
        let mut cursor = RowCursor::closed();
        let mut buffers = RowCursorBuffers::new(&config, &mut read_budget).unwrap();
        let mut arena = Arena::new(&mut read_budget, "row cursor output", 4096).unwrap();
        let metadata = Arena::new(&mut read_budget, "row cursor positions", 4096).unwrap();
        assert_eq!(read_budget.remaining(), 0);
        let mut tiny_budget = Budget::new(2);
        let tiny_metadata = Arena::new(&mut tiny_budget, "small cursor positions", 1).unwrap();
        let tiny_output = Arena::new(&mut tiny_budget, "small cursor output", 1).unwrap();
        assert_eq!(
            crate::mem::guard::forbid_alloc(|| cursor.open(&storage, table, 0, &tiny_metadata))
                .err()
                .unwrap()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert!(storage.heap.test_exclusive_write().is_ok());
        crate::mem::guard::forbid_alloc(|| cursor.open(&storage, table, 0, &metadata)).unwrap();
        assert_eq!(
            crate::mem::guard::forbid_alloc(|| cursor.next(
                &mut buffers,
                &metadata,
                &tiny_output,
                None
            ))
            .err()
            .unwrap()
            .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        storage.commit_drop(table);
        let replacement = storage
            .create_table(make_def("replacement", &[("n", ColType::Int4, true)]))
            .unwrap();
        assert_eq!(replacement, table);
        for expected in 1..=3 {
            arena.reset();
            let row = crate::mem::guard::forbid_alloc(|| {
                cursor.next(&mut buffers, &metadata, &arena, None)
            })
            .unwrap()
            .unwrap();
            assert_eq!(row.rowid, expected);
            let SpilledRowRepresentation::Encoded(bytes) = row.representation else {
                panic!("heap row")
            };
            let mut values = [Datum::Null];
            super::super::super::rowenc::decode(bytes, &[ColType::Int4], &mut values).unwrap();
            assert_eq!(values[0], Datum::Int4(expected as i32));
        }
        assert!(
            crate::mem::guard::forbid_alloc(|| cursor.next(&mut buffers, &metadata, &arena, None))
                .unwrap()
                .is_none()
        );
        assert!(
            cursor
                .next(&mut buffers, &metadata, &arena, None)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            crate::mem::guard::forbid_alloc(|| cursor.next(
                &mut buffers,
                &tiny_metadata,
                &arena,
                None
            ))
            .err()
            .unwrap()
            .sqlstate,
            sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE
        );
        cursor.rewind(&metadata).unwrap();
        assert_eq!(
            cursor
                .next(&mut buffers, &metadata, &arena, None)
                .unwrap()
                .unwrap()
                .rowid,
            1
        );
        cursor.close();
        assert!(storage.heap.test_exclusive_write().is_ok());
        assert_eq!(
            cursor
                .next(&mut buffers, &metadata, &arena, None)
                .err()
                .unwrap()
                .sqlstate,
            sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE
        );
    }
}
