//! A row-at-a-time scan retains selected versions and unfinished downloads.

use super::*;
use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;
use crate::storage::{RowReadBuffers, SpillWalkPosition, SpilledRow, SpilledRowRepresentation};

pub(in crate::storage) struct RowCursor {
    source: Option<TableRowSnapshot<'static>>,
    rows: FixedVec<CapturedRowRecord>,
    resident: usize,
    commit: u64,
    done: bool,
    spill: SpillWalkPosition,
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

impl RowCursor {
    pub(in crate::storage) fn allocation_bytes(config: &Config) -> usize {
        config.table_rows * core::mem::size_of::<CapturedRowRecord>()
            + SpillWalkPosition::allocation_bytes(config.max_spill_generations_per_table)
    }

    pub(in crate::storage) fn new(
        config: &Config,
        budget: &mut Budget,
    ) -> Result<Self, BudgetError> {
        let rows = FixedVec::new(
            budget,
            "query cursor resident selections",
            config.table_rows,
        )?;
        budget.draw(
            SpillWalkPosition::allocation_bytes(config.max_spill_generations_per_table),
            "query cursor member positions",
        )?;
        Ok(Self {
            source: None,
            rows,
            resident: 0,
            commit: 0,
            done: false,
            spill: SpillWalkPosition::new(config.max_spill_generations_per_table),
        })
    }

    pub(in crate::storage) fn close(&mut self) {
        self.source = None;
        self.rows.clear();
        self.resident = 0;
        self.commit = 0;
        self.done = false;
        self.spill.reset();
    }

    /// Capturing row identities and owners performs no object-store reads.
    pub(in crate::storage) fn open(
        &mut self,
        storage: &Storage,
        table: usize,
        txid: u32,
    ) -> Result<(), SqlError> {
        self.close();
        let commit = storage.commit_snapshot();
        let captured = storage.capture_byte_partition(
            table,
            txid,
            storage.read_snapshot(),
            commit,
            None,
            |count| {
                if count > self.rows.capacity() {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "query cursor resident selections exceed table_rows ({})",
                        self.rows.capacity()
                    ));
                }
                for _ in 0..count {
                    self.rows
                        .push(CapturedRowRecord::EMPTY)
                        .expect("checked row capacity");
                }
                Ok(self.rows.as_mut_slice())
            },
        );
        match captured {
            Ok((source, _, _)) => {
                self.source = Some(source);
                self.commit = commit;
                Ok(())
            }
            Err(error) => {
                self.close();
                Err(error)
            }
        }
    }

    pub(in crate::storage) fn next<'arena>(
        &mut self,
        buffers: &mut RowCursorBuffers,
        arena: &'arena Arena,
        decoded_columns: Option<&[bool; crate::storage::MAX_COLUMNS]>,
    ) -> Result<Option<SpilledRow<'arena>>, SqlError> {
        crate::sql::query::check_timeout()?;
        let source = self.source.as_ref().ok_or_else(|| {
            sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "query row cursor is closed"
            )
        })?;
        if self.done {
            return Ok(None);
        }
        while self.resident < self.rows.len() {
            let record = &mut self.rows[self.resident];
            source.resolve_saved(record, self.commit)?;
            let Some(home) = source.read_record(*record)? else {
                self.resident += 1;
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
            self.resident += 1;
            return Ok(Some(SpilledRow {
                rowid,
                representation: SpilledRowRepresentation::Encoded(bytes),
            }));
        }
        let mut row = None;
        let complete = Storage::spill_merged_walk_bytes(
            source.roots(),
            arena,
            false,
            decoded_columns,
            true,
            crate::storage::SpillOverlayMode::VisibleScan(&RowScanIds::Captured(&self.rows)),
            Some(self.spill.resume(&buffers.scan, 1, u64::MAX)),
            &mut |rowid, _, representation| {
                row = Some(SpilledRow {
                    rowid,
                    representation,
                });
                Ok(ControlFlow::Break(()))
            },
        )?;
        self.done = complete;
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
        storage.set_commit_snapshot(7);
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
        let mut read_budget = Budget::new(
            RowCursor::allocation_bytes(&config)
                + RowCursorBuffers::allocation_bytes(&config)
                + 4096,
        );
        let mut cursor = RowCursor::new(&config, &mut read_budget).unwrap();
        let mut buffers = RowCursorBuffers::new(&config, &mut read_budget).unwrap();
        let mut arena = Arena::new(&mut read_budget, "row cursor output", 4096).unwrap();
        assert_eq!(read_budget.remaining(), 0);
        crate::mem::guard::forbid_alloc(|| cursor.open(&storage, table, 0)).unwrap();
        storage.drop_table(table);
        let replacement = storage
            .create_table(make_def("replacement", &[("n", ColType::Int4, true)]))
            .unwrap();
        assert_eq!(replacement, table);
        for expected in 1..=3 {
            arena.reset();
            let row = crate::mem::guard::forbid_alloc(|| cursor.next(&mut buffers, &arena, None))
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
            crate::mem::guard::forbid_alloc(|| cursor.next(&mut buffers, &arena, None))
                .unwrap()
                .is_none()
        );
        assert!(cursor.next(&mut buffers, &arena, None).unwrap().is_none());
        cursor.close();
        assert_eq!(
            cursor
                .next(&mut buffers, &arena, None)
                .err()
                .unwrap()
                .sqlstate,
            sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE
        );
    }
}
