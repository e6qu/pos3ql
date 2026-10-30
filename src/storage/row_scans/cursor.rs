//! A row-at-a-time scan retains selected versions and unfinished downloads.

use super::*;
use crate::config::Config;
use crate::mem::arena::{ArenaSliceError, ArenaSliceHandle};
use crate::mem::budget::{Budget, BudgetError};
use crate::storage::{
    HeapRowRead, MemberCursor, RowReadBuffers, SpillWalkProgress, SpilledRowRepresentation,
};

pub(crate) struct CursorRow<'a> {
    pub(crate) rowid: u64,
    bytes: CursorRowBytes<'a>,
}

enum CursorRowBytes<'a> {
    Heap(HeapRowRead),
    Spilled(SpilledRowRepresentation<'a>),
}

impl CursorRow<'_> {
    pub(crate) fn representation(&self) -> SpilledRowRepresentation<'_> {
        match &self.bytes {
            CursorRowBytes::Heap(bytes) => SpilledRowRepresentation::Encoded(bytes),
            CursorRowBytes::Spilled(representation) => *representation,
        }
    }
}

pub(crate) struct RowCursor {
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

pub(crate) struct RowCursorBuffers {
    scan: crate::mem::mutex::Mutex<crate::storage::ScanContext>,
    row: RowReadBuffers,
}

impl RowCursorBuffers {
    pub(crate) fn allocation_bytes(config: &Config) -> usize {
        crate::storage::ScanContext::budget_bytes(config.max_spill_generations_per_table)
            - core::mem::size_of::<crate::mem::mutex::Mutex<crate::storage::ScanContext>>()
            + RowReadBuffers::budget_bytes()
            - core::mem::size_of::<RowReadBuffers>()
    }

    pub(crate) fn new(config: &Config, budget: &mut Budget) -> Result<Self, BudgetError> {
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

fn cursor_capacity(error: crate::mem::arena::ArenaFull) -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "query cursor memory exhausted: {}",
        error
    )
}

impl RowCursor {
    #[cfg(test)]
    pub(crate) fn position_bytes(rows: usize, members: usize) -> Option<usize> {
        Arena::retained_slice_capacity::<CapturedRowRecord>(rows)?
            .checked_add(Arena::retained_slice_capacity::<MemberCursor>(members)?)
    }

    pub(crate) const fn closed() -> Self {
        Self { source: None }
    }

    pub(crate) fn close(&mut self) {
        self.source = None;
    }

    /// Capturing row identities and owners performs no object-store reads.
    pub(crate) fn open(
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
                    .map_err(cursor_capacity)?;
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
            .map_err(cursor_capacity)?;
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

    #[cfg(test)]
    pub(crate) fn rewind(&mut self, arena: &Arena) -> Result<(), SqlError> {
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

    pub(crate) fn next<'arena>(
        &mut self,
        buffers: &mut RowCursorBuffers,
        state_arena: &Arena,
        arena: &'arena Arena,
        decoded_columns: Option<&[bool; crate::storage::MAX_COLUMNS]>,
    ) -> Result<Option<CursorRow<'arena>>, SqlError> {
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
                    .map_err(cursor_capacity)
            };
            let bytes = match home.bytes {
                crate::storage::row_images::VisibleRowBytes::Heap(bytes) => {
                    CursorRowBytes::Heap(bytes)
                }
                crate::storage::row_images::VisibleRowBytes::Spilled(row) => {
                    CursorRowBytes::Spilled(SpilledRowRepresentation::Encoded(
                        row.with_bytes_in(&mut buffers.row, copy)?,
                    ))
                }
            };
            let rowid = record.rowid;
            source.resident += 1;
            return Ok(Some(CursorRow { rowid, bytes }));
        }
        let mut members = state_arena
            .retained_slice_mut(source.members)
            .map_err(state_error)?;
        // The bounded walk can consume a tombstone or a shadowed saved row
        // without producing a visible row. Keep its progress and ask again.
        loop {
            crate::sql::query::check_timeout()?;
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
                    row = Some(CursorRow {
                        rowid,
                        bytes: CursorRowBytes::Spilled(representation),
                    });
                    Ok(ControlFlow::Break(()))
                },
            )?;
            source.done = complete;
            if row.is_some() || complete {
                return Ok(row);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::types::{ColType, Datum};
    use crate::storage::RowState;
    use crate::storage::tests::{make_def, test_budget, test_config};

    #[test]
    fn cursor_owned_query_retains_a_physical_plan_after_owner_and_storage_moves() {
        use crate::mem::FixedBuf;
        use crate::pg::respond::Responder;
        use crate::sql::cursor::CursorQuery;

        // Exercise real owners with minimal unused catalog capacity under
        // Miri's interpreter, including the required built-in text objects.
        let mut config = test_config();
        config.memtable_bytes = 256;
        config.max_tables = 2;
        config.table_rows = 2;
        config.txn_rows = 2;
        config.max_catalog_versions_per_object = 1;
        config.catalog_history_bytes = 4096;
        config.max_spill_generations_per_table = 1;
        config.max_views = 1;
        config.max_materialized_views = 1;
        config.max_routines = 1;
        config.max_casts = 1;
        config.max_operators = 1;
        config.max_operator_families = 1;
        config.max_operator_classes = 1;
        config.max_composites = 1;
        config.max_domains = 1;
        config.max_enums = 1;
        config.max_triggers = 1;
        config.max_policies = 1;
        config.max_publications = 1;
        config.max_text_search_objects = 21;
        config.max_large_objects = 1;
        config.large_object_pages = 1;
        config.max_extended_statistics = 1;
        config.max_subscriptions = 1;
        config.max_foreign_data_wrappers = 1;
        config.max_foreign_servers = 1;
        config.max_user_mappings = 1;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let visibility = storage.nested_statement_snapshot(7);
        let table = storage
            .create_table(make_def(
                "retained_plan",
                &[
                    ("n", ColType::Int4, true),
                    ("payload", ColType::Text, false),
                ],
            ))
            .unwrap();
        for rowid in 1..=2 {
            let location = storage
                .heap
                .append_row(&[Datum::Int4(rowid as i32), Datum::Text("value")])
                .unwrap();
            storage.tables[table]
                .rows
                .insert(rowid, RowState::committed_only_at(location, 7))
                .unwrap();
        }
        let mut read_budget = Budget::new(RowCursorBuffers::allocation_bytes(&config) + (1 << 18));
        let mut query = CursorQuery::new(&mut read_budget, 1 << 16).unwrap();
        let work = Arena::new(&mut read_budget, "retained projection work", 1 << 16).unwrap();
        let mut buffers = RowCursorBuffers::new(&config, &mut read_budget).unwrap();
        let mut description =
            FixedBuf::new(&mut read_budget, "retained description", 1024).unwrap();
        let mut rows = FixedBuf::new(&mut read_budget, "retained rows", 1024).unwrap();
        let mut small_rows = FixedBuf::new(&mut read_budget, "small retained rows", 1).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            query
                .bind("SELECT n+0,payload FROM retained_plan WHERE n>0", &[], &[])
                .unwrap();
            assert!(
                query
                    .prepare_source(
                        &storage,
                        0,
                        &mut Responder::for_cursor(
                            &mut description,
                            crate::pg::respond::CursorRowOutput::Save(&mut rows)
                        )
                    )
                    .unwrap()
            );
        });
        assert!(rows.readable().is_empty());
        let error = crate::mem::guard::forbid_alloc(|| {
            query.advance_source(
                &storage,
                0,
                0,
                &work,
                &mut buffers,
                &mut Responder::for_cursor(
                    &mut description,
                    crate::pg::respond::CursorRowOutput::Save(&mut small_rows),
                ),
            )
        })
        .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        // The pending row must own its bytes across movement and another edit.
        let mut query = Box::new(query);
        let mut storage = Box::new(storage);
        // A retained plan must not borrow the Storage object or its movable heap.
        storage
            .create_table(make_def("another_relation", &[("n", ColType::Int4, true)]))
            .unwrap();
        for expected in 1..=2 {
            rows.clear();
            crate::mem::guard::forbid_alloc(|| {
                assert!(
                    query
                        .advance_source(
                            &storage,
                            0,
                            0,
                            &work,
                            &mut buffers,
                            &mut Responder::for_cursor(
                                &mut description,
                                crate::pg::respond::CursorRowOutput::Save(&mut rows)
                            )
                        )
                        .unwrap()
                )
            });
            let bytes = rows.readable();
            let length = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
            assert_eq!(bytes.len(), length + 4);
            let mut decoded = [Datum::Null; 2];
            crate::sql::exec::decode_projected_row_into(&bytes[4..], &work, &mut decoded).unwrap();
            assert_eq!(decoded, [Datum::Int4(expected), Datum::Text("value")]);
        }
        assert!(
            !query
                .advance_source(
                    &storage,
                    0,
                    0,
                    &work,
                    &mut buffers,
                    &mut Responder::for_cursor(
                        &mut description,
                        crate::pg::respond::CursorRowOutput::Save(&mut rows)
                    )
                )
                .unwrap()
        );
        crate::mem::guard::forbid_alloc(|| query.clear());
        assert!(storage.heap.test_exclusive_write().is_ok());
        drop(visibility);
    }

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
        let row = crate::mem::guard::forbid_alloc(|| {
            cursor.next(&mut buffers, &metadata, &tiny_output, None)
        })
        .unwrap()
        .unwrap();
        assert_eq!(row.rowid, 1);
        assert_eq!(
            tiny_output.used(),
            0,
            "resident bytes already have an immutable owner"
        );
        drop(row);
        cursor.rewind(&metadata).unwrap();
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
            let SpilledRowRepresentation::Encoded(bytes) = row.representation() else {
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
