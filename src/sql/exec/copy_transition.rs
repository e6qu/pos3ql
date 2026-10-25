//! COPY keeps inserted values needed by statement triggers across messages.

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
}

impl CopyTransitionRows {
    pub(crate) const fn budget_bytes(config: &Config) -> usize {
        config.txn_rows * core::mem::size_of::<EncodedRow>() + config.copy_transition_bytes
    }

    pub(crate) fn new(budget: &mut Budget, rows: usize, bytes: usize) -> Result<Self, BudgetError> {
        Ok(Self {
            rows: FixedVec::new(budget, "copy_transition_rows", rows)?,
            bytes: FixedBuf::new(budget, "copy_transition_bytes", bytes)?,
        })
    }

    pub(crate) fn clear(&mut self) {
        self.rows.clear();
        self.bytes.clear();
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::types::ColType;

    #[test]
    fn copy_transition_values_reserve_the_exact_startup_budget() {
        let config = Config::parse(
            "txn_rows = 3\ncopy_transition_bytes = 73\ntransaction_mode = mvcc\ncatalog_history_bytes = 16777216\n",
        ).unwrap();
        assert_eq!(config.copy_transition_bytes, 73);
        let required = CopyTransitionRows::budget_bytes(&config);
        let mut short = Budget::new(required - 1);
        assert!(CopyTransitionRows::new(&mut short, 3, 73).is_err());
        let mut exact = Budget::new(required);
        let rows = CopyTransitionRows::new(&mut exact, 3, 73).unwrap();
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
    fn copy_transition_values_own_bytes_and_retry_without_growing() {
        let mut budget = Budget::new(96);
        let mut rows = CopyTransitionRows::new(&mut budget, 2, 64).unwrap();
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
