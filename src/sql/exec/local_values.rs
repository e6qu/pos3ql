//! Saved values own reusable byte ranges independently of every statement arena.

use super::*;
pub(super) struct SavedValue {
    bytes: MutableSlice<u8>,
    length: Option<usize>,
}

impl SavedValue {
    pub(super) fn new(pool: &std::sync::Arc<LeasePool>) -> Result<Self, SqlError> {
        Ok(Self {
            bytes: pool.lease(0).map_err(exhausted)?,
            length: None,
        })
    }

    pub(super) fn replace(&mut self, value: Datum<'_>) -> Result<(), SqlError> {
        // Validate the complete value before changing a previously saved image.
        let length = projected_row_len(&[value])?;
        let capacity = length
            .checked_next_power_of_two()
            .ok_or_else(full)?
            .min(self.bytes.max_entries())
            .max(length);
        self.bytes.reserve_exact(capacity).map_err(exhausted)?;
        const ZEROES: [u8; 4096] = [0; 4096];
        while self.bytes.len() < length {
            super::super::query::check_timeout()?;
            let count = (length - self.bytes.len()).min(ZEROES.len());
            self.bytes
                .extend_from_slice(&ZEROES[..count])
                .map_err(exhausted)?;
        }
        // Encoding repeats the same validation before its first write. After
        // successful sizing there is no fallible conversion or helper call.
        encode_projected_into(&[value], &mut self.bytes[..length])?;
        self.length = Some(length);
        Ok(())
    }

    pub(super) fn read<'out>(&self, output: &'out Arena) -> Result<Datum<'out>, SqlError> {
        let length = self.length.ok_or_else(inactive)?;
        let copied = output
            .alloc_slice_copy(&self.bytes[..length])
            .map_err(|_| full())?;
        decode_projected_col_record(copied, 0, output)
    }
}

fn exhausted(error: crate::mem::lease_pool::LeaseError) -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "PL/pgSQL saved values: {}",
        error
    )
}

fn full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "PL/pgSQL saved values exceed statement memory"
    )
}

fn inactive() -> SqlError {
    sql_err!(
        sqlstate::INTERNAL_ERROR,
        "PL/pgSQL saved value has not been initialized"
    )
}

#[cfg(test)]
mod ownership_tests;
