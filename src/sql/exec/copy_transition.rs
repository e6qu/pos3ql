//! COPY keeps, across its data messages, the inserted values its statement
//! triggers need and the rows it queued for batched foreign INSERTs.

use crate::config::Config;
use crate::mem::FixedBuf;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::types::Datum;
use crate::sql_err;
use crate::storage::rowenc;

#[derive(Clone, Copy)]
struct EncodedRow {
    offset: usize,
    length: usize,
}

pub(crate) struct CopyTransitionRows {
    rows: FixedVec<EncodedRow>,
    bytes: FixedBuf,
    /// Rows waiting for the buffering window's flush to send them in
    /// `batch_size` remote INSERTs.
    pub(super) foreign: CopyForeignRows,
}

/// One COPY row queued for a foreign table: its values as Bind fields (a
/// big-endian length, -1 for NULL, then the text) in `CopyForeignRows::bytes`.
#[derive(Clone, Copy)]
pub(super) struct QueuedForeignRow {
    pub(super) table: usize,
    offset: usize,
    length: usize,
}

/// The foreign rows of COPY FROM's buffering window (postgres_fdw's
/// `ExecForeignBatchInsert` under `CopyMultiInsertBufferFlush`): at most
/// [`super::copy_batch::MAX_BUFFERED_TUPLES`] rows, in a byte queue of
/// `foreign_send_bytes` reserved at startup.
pub(crate) struct CopyForeignRows {
    rows: FixedVec<QueuedForeignRow>,
    bytes: FixedBuf,
}

impl CopyForeignRows {
    const fn budget_bytes(foreign_bytes: usize) -> usize {
        (super::copy_batch::MAX_BUFFERED_TUPLES * core::mem::size_of::<QueuedForeignRow>())
            .saturating_add(foreign_bytes)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub(super) fn rows(&self) -> &[QueuedForeignRow] {
        self.rows.as_slice()
    }

    pub(super) fn fields(&self, row: QueuedForeignRow) -> &[u8] {
        &self.bytes.readable()[row.offset..row.offset + row.length]
    }

    /// Queues `fields` for `table`.
    pub(super) fn push(&mut self, table: usize, fields: &[u8]) -> Result<(), SqlError> {
        let offset = self.bytes.len();
        if self.rows.len() == self.rows.capacity() || fields.len() > self.bytes.capacity() - offset
        {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "COPY rows queued for foreign tables exceed foreign_send_bytes ({})",
                self.bytes.capacity()
            ));
        }
        self.bytes.writable()[..fields.len()].copy_from_slice(fields);
        self.bytes.advance(fields.len());
        self.rows
            .push(QueuedForeignRow {
                table,
                offset,
                length: fields.len(),
            })
            .expect("capacity checked before copying");
        Ok(())
    }

    pub(super) fn clear(&mut self) {
        self.rows.clear();
        self.bytes.clear();
    }
}

impl CopyTransitionRows {
    pub(crate) const fn budget_bytes(config: &Config) -> usize {
        config
            .txn_rows
            .saturating_mul(core::mem::size_of::<EncodedRow>())
            .saturating_add(config.copy_transition_bytes)
            .saturating_add(CopyForeignRows::budget_bytes(config.foreign_send_bytes))
    }

    pub(crate) fn new(
        budget: &mut Budget,
        rows: usize,
        bytes: usize,
        foreign_bytes: usize,
    ) -> Result<Self, BudgetError> {
        Ok(Self {
            rows: FixedVec::new(budget, "copy_transition_rows", rows)?,
            bytes: FixedBuf::new(budget, "copy_transition_bytes", bytes)?,
            foreign: CopyForeignRows {
                rows: FixedVec::new(
                    budget,
                    "copy_foreign_rows",
                    super::copy_batch::MAX_BUFFERED_TUPLES,
                )?,
                bytes: FixedBuf::new(budget, "copy_foreign_bytes", foreign_bytes)?,
            },
        })
    }

    pub(crate) fn clear(&mut self) {
        self.rows.clear();
        self.bytes.clear();
        self.foreign.clear();
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }

    pub(super) fn capture(&mut self, values: &[Datum<'_>]) -> Result<(), SqlError> {
        if self.rows.len() == self.rows.capacity() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "COPY transition capture exceeds {} rows",
                self.rows.capacity()
            ));
        }
        let length = rowenc::encoded_len(values);
        let offset = self.bytes.len();
        if length > self.bytes.capacity() - offset {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "COPY transition rows exceed copy_transition_bytes ({})",
                self.bytes.capacity()
            ));
        }
        rowenc::encode(values, &mut self.bytes.writable()[..length]);
        self.bytes.advance(length);
        self.rows
            .push(EncodedRow { offset, length })
            .expect("capacity checked before copying");
        Ok(())
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &[u8]> {
        self.rows
            .iter()
            .map(|row| &self.bytes.readable()[row.offset..row.offset + row.length])
    }

    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.rows.capacity()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

#[cfg(all(
    test,
    not(all(miri, feature = "miri-ownership")),
    not(feature = "sql-fixture-suite")
))]
mod tests {
    use super::*;
    use crate::sql::types::ColType;

    #[test]
    fn configured_copy_transition_budget_saturates_before_wrapping() {
        let mut config = Config::default_dev();
        config.txn_rows = usize::MAX;
        assert_eq!(CopyTransitionRows::budget_bytes(&config), usize::MAX);
        config.txn_rows = 1;
        config.copy_transition_bytes = usize::MAX;
        assert_eq!(CopyTransitionRows::budget_bytes(&config), usize::MAX);
    }

    #[test]
    fn copy_transition_values_reserve_the_exact_startup_budget() {
        let config = Config::parse(
            "txn_rows = 3\ncopy_transition_bytes = 73\ntransaction_mode = mvcc\ncatalog_history_bytes = 16777216\nrow_history_slots = 65536\nrow_overlay_slots = 8192\nrow_work_bytes = 8388608\nrow_work_leases = 128\nexecution_control_bytes = 262144\nexecution_control_leases = 256\ndefinition_image_slots = 256\ntable_metadata_bytes = 64MiB\ntable_metadata_leases = 8192\nstatistics_bytes = 1MiB\nstatistics_leases = 1024\nresult_spool_bytes = 16MiB\nresult_spool_leases = 64\nplan_build_bytes = 16MiB\nplan_build_leases = 64\nresult_stream_bytes = 256KiB\nresult_stream_slots = 2\nindex_statistics_slots = 4096\ntrigger_event_spill_bytes = 0\n",
        ).unwrap();
        assert_eq!(config.copy_transition_bytes, 73);
        let required = CopyTransitionRows::budget_bytes(&config);
        let mut short = Budget::new(required - 1);
        assert!(CopyTransitionRows::new(&mut short, 3, 73, config.foreign_send_bytes).is_err());
        let mut exact = Budget::new(required);
        let rows = CopyTransitionRows::new(&mut exact, 3, 73, config.foreign_send_bytes).unwrap();
        assert_eq!(exact.remaining(), 0);
        assert_eq!(rows.capacity(), 3);
        assert_eq!(rows.bytes.capacity(), 73);
        let mut larger = config.clone();
        larger.copy_transition_bytes += 19;
        assert_eq!(
            larger.memory_plan(0, 0).connections - config.memory_plan(0, 0).connections,
            19 * config.max_connections as usize
        );
        assert_eq!(
            crate::pg::subscription_apply::SubscriptionApply::budget_bytes(&larger)
                - crate::pg::subscription_apply::SubscriptionApply::budget_bytes(&config),
            19
        );
    }

    #[test]
    fn copy_foreign_rows_queue_in_their_startup_buffer_and_fail_named() {
        let mut budget = Budget::new(
            core::mem::size_of::<EncodedRow>() + 16 + CopyForeignRows::budget_bytes(10),
        );
        let mut rows = CopyTransitionRows::new(&mut budget, 1, 16, 10).unwrap();
        assert_eq!(budget.remaining(), 0);
        crate::mem::guard::forbid_alloc(|| {
            rows.foreign.push(3, b"\0\0\0\x01a").unwrap();
            let error = rows.foreign.push(4, b"\0\0\0\x02bb").unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(error.message.as_str().contains("foreign_send_bytes (10)"));
        });
        let queued = rows.foreign.rows()[0];
        assert_eq!(queued.table, 3);
        assert_eq!(rows.foreign.fields(queued), b"\0\0\0\x01a");
        rows.clear();
        assert!(rows.foreign.is_empty());
    }

    #[test]
    fn copy_transition_values_own_bytes_and_retry_without_growing() {
        let mut budget = Budget::new(96 + CopyForeignRows::budget_bytes(16));
        let mut rows = CopyTransitionRows::new(&mut budget, 2, 64, 16).unwrap();
        let mut input = *b"first";
        crate::mem::guard::forbid_alloc(|| {
            rows.capture(&[
                Datum::Int4(1),
                Datum::Text(core::str::from_utf8(&input).unwrap()),
            ])
            .unwrap();
            input.fill(b'x');
            let original_bytes = rows.bytes.len();
            let oversized = [b'y'; 80];
            let error = rows
                .capture(&[
                    Datum::Int4(2),
                    Datum::Text(core::str::from_utf8(&oversized).unwrap()),
                ])
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(error.message.as_str().contains("copy_transition_bytes"));
            assert_eq!(rows.len(), 1);
            assert_eq!(rows.bytes.len(), original_bytes);
            let mut decoded = [Datum::Null; 2];
            rowenc::decode(
                rows.iter().next().unwrap(),
                &[ColType::Int4, ColType::Text],
                &mut decoded,
            )
            .unwrap();
            assert_eq!(decoded, [Datum::Int4(1), Datum::Text("first")]);
            rows.capture(&[Datum::Int4(2), Datum::Text("two")]).unwrap();
            let used = rows.bytes.len();
            let error = rows.capture(&[Datum::Int4(3)]).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(error.message.as_str().contains("exceeds 2 rows"));
            assert_eq!(rows.bytes.len(), used);
            rows.clear();
            rows.capture(&[Datum::Int4(9), Datum::Text("retry")])
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows.rows[0].offset, 0);
            assert_eq!(rows.bytes.capacity(), 64);
        });
        assert_eq!(budget.remaining(), 0);
    }
}
