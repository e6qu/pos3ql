//! Materialized loop inputs own encoded values in the fixed shared work pool.

use super::*;
use crate::mem::lease_pool::SharedSlice;
use std::sync::Arc;

pub(super) struct LoopRows {
    bytes: SharedSlice<u8>,
    count: usize,
    /// The query's column count, which a scalar variable is checked against.
    width: usize,
}

impl LoopRows {
    pub(super) fn capture(
        pool: &Arc<LeasePool>,
        rows: TriggerForRows<'_>,
    ) -> Result<Self, SqlError> {
        super::super::query::check_timeout()?;
        // Entry zero keeps the declared null row of an empty query. Entries
        // one onward are actual input values; the final offset ends the last.
        let offsets = rows
            .values
            .len()
            .checked_add(2)
            .and_then(|count| count.checked_mul(8))
            .ok_or_else(too_large)?;
        let mut length = offsets
            .checked_add(projected_row_len(&[rows.empty])?)
            .ok_or_else(too_large)?;
        for value in rows.values {
            super::super::query::check_timeout()?;
            length = length
                .checked_add(projected_row_len(&[*value])?)
                .ok_or_else(too_large)?;
        }
        let mut bytes = pool.lease::<u8>(length).map_err(full)?;
        const ZEROES: [u8; 4096] = [0; 4096];
        while bytes.len() < length {
            super::super::query::check_timeout()?;
            let count = (length - bytes.len()).min(ZEROES.len());
            bytes.extend_from_slice(&ZEROES[..count]).map_err(full)?;
        }
        let mut position = offsets;
        for (index, value) in core::iter::once(rows.empty)
            .chain(rows.values.iter().copied())
            .enumerate()
        {
            super::super::query::check_timeout()?;
            bytes[index * 8..(index + 1) * 8].copy_from_slice(&(position as u64).to_le_bytes());
            let length = projected_row_len(&[value])?;
            encode_projected_into(&[value], &mut bytes[position..position + length])?;
            position += length;
        }
        bytes[(rows.values.len() + 1) * 8..offsets]
            .copy_from_slice(&(position as u64).to_le_bytes());
        assert_eq!(position, length);
        Ok(Self {
            bytes: bytes.freeze(),
            count: rows.values.len(),
            width: rows.width,
        })
    }

    pub(super) fn len(&self) -> usize {
        self.count
    }

    pub(super) fn width(&self) -> usize {
        self.width
    }

    pub(super) fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub(super) fn empty_query_row<'a>(&self, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
        self.decode(0, arena)
    }

    pub(super) fn value<'a>(&self, index: usize, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
        assert!(index < self.count, "materialized loop row index");
        self.decode(index + 1, arena)
    }

    fn decode<'a>(&self, index: usize, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
        let offset = |index: usize| {
            usize::try_from(u64::from_le_bytes(
                self.bytes[index * 8..(index + 1) * 8]
                    .try_into()
                    .expect("loop row offset width"),
            ))
            .expect("in-process loop row offset")
        };
        let encoded = arena
            .alloc_slice_copy(&self.bytes[offset(index)..offset(index + 1)])
            .map_err(|_| super::super::query::arena_full_pub())?;
        decode_projected_col_record(encoded, 0, arena)
    }
}

fn full(error: crate::mem::lease_pool::LeaseError) -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "PL/pgSQL loop rows: {}",
        error
    )
}

fn too_large() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "PL/pgSQL loop rows exceed addressable work memory"
    )
}

#[cfg(test)]
mod ownership_tests;
