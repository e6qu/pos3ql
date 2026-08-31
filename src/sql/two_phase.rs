//! Startup-bounded PostgreSQL prepared-transaction ownership.

use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::buffer::FixedBuf;
use crate::sql::ast::PreparedTransactionId;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::txn::TxnState;
use crate::sql_err;
use crate::storage::DatabaseOid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PreparedTransactionMetadata {
    pub gid: PreparedTransactionId,
    pub transaction_id: u32,
    pub owner: u16,
    pub database: DatabaseOid,
    pub prepared_at: i64,
    pub first_lsn: u64,
    pub prepared_lsn: u64,
}

pub(crate) struct PreparedTransactionSlot {
    metadata: Option<PreparedTransactionMetadata>,
    /// Live transactions retain their exact transaction-owned catalog/row
    /// overlays and locks here after detaching from the preparing connection.
    pub transaction: TxnState,
    /// Final WAL records preceding the typed prepare marker. Recovery uses the
    /// same bytes to rebuild a prepared transaction or resolve it later.
    pub records: FixedBuf,
    pub locks: FixedBuf,
    pub recovered: bool,
}

pub(crate) struct PreparedTransactions {
    slots: Vec<PreparedTransactionSlot>,
}

impl PreparedTransactions {
    pub(crate) fn budget_bytes(config: &Config) -> usize {
        config.max_prepared_transactions
            * (core::mem::size_of::<PreparedTransactionSlot>()
                + TxnState::budget_bytes_with_large_objects(
                    config.txn_rows,
                    config.max_large_object_descriptors,
                )
                + 2 * config.wal_buffer_bytes)
    }

    pub(crate) fn new(config: &Config, budget: &mut Budget) -> Result<Self, BudgetError> {
        budget.draw_array(
            config.max_prepared_transactions,
            core::mem::size_of::<PreparedTransactionSlot>(),
            "prepared transaction slots",
        )?;
        let mut slots = Vec::with_capacity(config.max_prepared_transactions);
        for _ in 0..config.max_prepared_transactions {
            slots.push(PreparedTransactionSlot {
                metadata: None,
                transaction: TxnState::new_with_large_objects(
                    budget,
                    config.txn_rows,
                    config.max_large_object_descriptors,
                )?,
                records: FixedBuf::new(
                    budget,
                    "prepared transaction WAL",
                    config.wal_buffer_bytes,
                )?,
                locks: FixedBuf::new(
                    budget,
                    "prepared transaction locks",
                    config.wal_buffer_bytes,
                )?,
                recovered: false,
            });
        }
        Ok(Self { slots })
    }

    pub(crate) fn capacity(&self) -> usize {
        self.slots.len()
    }

    pub(crate) fn find(&self, gid: PreparedTransactionId) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| slot.metadata.is_some_and(|metadata| metadata.gid == gid))
    }

    pub(crate) fn reserve(&mut self, metadata: PreparedTransactionMetadata) -> Option<usize> {
        let index = self.slots.iter().position(|slot| slot.metadata.is_none())?;
        let slot = &mut self.slots[index];
        slot.metadata = Some(metadata);
        slot.records.clear();
        slot.locks.clear();
        slot.recovered = false;
        Some(index)
    }

    pub(crate) fn slot(&self, index: usize) -> &PreparedTransactionSlot {
        &self.slots[index]
    }

    pub(crate) fn slot_mut(&mut self, index: usize) -> &mut PreparedTransactionSlot {
        &mut self.slots[index]
    }

    pub(crate) fn set_lsn_range(&mut self, index: usize, first_lsn: u64, prepared_lsn: u64) {
        let metadata = self.slots[index]
            .metadata
            .as_mut()
            .expect("prepared transaction slot is occupied");
        metadata.first_lsn = first_lsn;
        metadata.prepared_lsn = prepared_lsn;
    }

    pub(crate) fn entries(
        &self,
    ) -> impl Iterator<Item = (usize, PreparedTransactionMetadata)> + '_ {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.metadata.map(|metadata| (index, metadata)))
    }

    pub(crate) fn release(&mut self, index: usize) {
        let slot = &mut self.slots[index];
        slot.metadata = None;
        slot.transaction.clear();
        slot.records.clear();
        slot.locks.clear();
        slot.recovered = false;
    }
}

impl PreparedTransactionSlot {
    pub(crate) fn metadata(&self) -> PreparedTransactionMetadata {
        self.metadata
            .expect("prepared transaction slot is occupied")
    }

    pub(crate) fn push_record(&mut self, lsn: u64, raw: &[u8]) -> Result<(), SqlError> {
        if raw.len() > u32::MAX as usize
            || !self.records.append(&lsn.to_le_bytes())
            || !self.records.append(&(raw.len() as u32).to_le_bytes())
            || !self.records.append(raw)
        {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "prepared transaction exceeds its startup WAL capacity"
            ));
        }
        Ok(())
    }

    pub(crate) fn first_lsn(&self) -> Option<u64> {
        self.records
            .readable()
            .get(..8)
            .map(|bytes| u64::from_le_bytes(bytes.try_into().unwrap()))
    }

    pub(crate) fn visit_records(
        &self,
        mut visit: impl FnMut(u64, &[u8]) -> Result<(), SqlError>,
    ) -> Result<(), SqlError> {
        let bytes = self.records.readable();
        let mut at = 0usize;
        while at < bytes.len() {
            let lsn = u64::from_le_bytes(
                bytes
                    .get(at..at + 8)
                    .ok_or_else(corrupt_records)?
                    .try_into()
                    .unwrap(),
            );
            at += 8;
            let length = u32::from_le_bytes(
                bytes
                    .get(at..at + 4)
                    .ok_or_else(corrupt_records)?
                    .try_into()
                    .unwrap(),
            ) as usize;
            at += 4;
            let raw = bytes.get(at..at + length).ok_or_else(corrupt_records)?;
            at += length;
            visit(lsn, raw)?;
        }
        Ok(())
    }
}

fn corrupt_records() -> SqlError {
    sql_err!(
        sqlstate::DATA_EXCEPTION,
        "prepared transaction WAL is corrupt"
    )
}
