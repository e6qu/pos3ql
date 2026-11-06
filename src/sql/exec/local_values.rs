//! Saved values own reusable encoded bytes, independently of expression scratch.

use super::*;
use crate::mem::arena::ArenaSliceHandle;

pub(super) struct SavedValue {
    bytes: Option<ArenaSliceHandle<u8>>,
    capacity: usize,
    length: usize,
}

impl SavedValue {
    pub(super) const fn new() -> Self {
        Self {
            bytes: None,
            capacity: 0,
            length: 0,
        }
    }

    pub(super) fn replace(&mut self, value: Datum<'_>, arena: &Arena) -> Result<(), SqlError> {
        self.require_owner(arena)?;
        // Validate the complete value before changing a previously saved image.
        let length = projected_row_len(&[value])?;
        let (handle, capacity) = if length > self.capacity {
            let capacity = length.checked_next_power_of_two().ok_or_else(full)?;
            let handle = arena
                .retain_slice_with(capacity, |_| 0u8)
                .map_err(|_| full())?;
            (handle, capacity)
        } else {
            (self.bytes.ok_or_else(inactive)?, self.capacity)
        };
        let mut bytes = arena.retained_slice_mut(handle).map_err(|_| inactive())?;
        // Encoding repeats the same validation before its first write. After
        // successful sizing there is no fallible conversion or helper call.
        encode_projected_into(&[value], &mut bytes[..length])?;
        self.bytes = Some(handle);
        self.capacity = capacity;
        self.length = length;
        Ok(())
    }

    pub(super) fn require_owner(&self, arena: &Arena) -> Result<(), SqlError> {
        if let Some(handle) = self.bytes {
            // Validate even when a nested shadow keeps the bytes unchanged.
            drop(arena.retained_slice_mut(handle).map_err(|_| inactive())?);
        }
        Ok(())
    }

    pub(super) fn read<'out>(
        &self,
        source: &Arena,
        output: &'out Arena,
    ) -> Result<Datum<'out>, SqlError> {
        let bytes = source
            .retained_slice_mut(self.bytes.ok_or_else(inactive)?)
            .map_err(|_| inactive())?;
        let copied = output
            .alloc_slice_copy(&bytes[..self.length])
            .map_err(|_| full())?;
        drop(bytes);
        decode_projected_col_record(copied, 0, output)
    }
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
        "PL/pgSQL saved value storage was reset or replaced"
    )
}

#[cfg(test)]
mod ownership_tests;
