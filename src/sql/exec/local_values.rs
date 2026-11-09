//! Saved values own reusable byte ranges independently of every statement arena.

use super::*;
pub(super) struct SavedValues {
    bytes: MutableSlice<u8>,
    length: usize,
    width: Option<usize>,
}

impl SavedValues {
    pub(super) fn pool(&self) -> &std::sync::Arc<LeasePool> {
        self.bytes.pool()
    }

    pub(super) fn new(pool: &std::sync::Arc<LeasePool>) -> Result<Self, SqlError> {
        Ok(Self {
            bytes: pool.lease(0).map_err(exhausted)?,
            length: 0,
            width: None,
        })
    }

    pub(super) fn replace(&mut self, values: &[Datum<'_>]) -> Result<(), SqlError> {
        self.replace_indexed(values.len(), |index| values[index])
    }

    /// Reads already evaluated values by index, without staging a maximum-width
    /// array when a saved call combines arguments and both transition rows.
    pub(super) fn replace_indexed<'value>(
        &mut self,
        width: usize,
        value_at: impl Fn(usize) -> Datum<'value>,
    ) -> Result<(), SqlError> {
        // Validate the complete value before changing a previously saved image.
        if width == 0 {
            self.length = 0;
            self.width = Some(0);
            return Ok(());
        }
        let length = projected_row_len_by(width, &value_at)?;
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
        encode_projected_by_into(width, value_at, &mut self.bytes[..length])?;
        self.length = length;
        self.width = Some(width);
        Ok(())
    }

    #[expect(
        clippy::mut_from_ref,
        reason = "returns fresh disjoint arena slots; saved bytes are only copied"
    )]
    pub(super) fn decode<'out>(
        &self,
        output: &'out Arena,
    ) -> Result<&'out mut [Datum<'out>], SqlError> {
        let mark = output.mark();
        let result = (|| {
            let width = self.width.ok_or_else(inactive)?;
            let values = output
                .alloc_slice_with(width, |_| Datum::Null)
                .map_err(|_| full())?;
            self.restore(values, output)?;
            Ok(values)
        })();
        if result.is_err() {
            // SAFETY: no slot or decoded payload escapes a failed admission.
            unsafe { output.rewind_to(mark) };
        }
        result
    }

    pub(super) fn read<'out>(
        &self,
        index: usize,
        output: &'out Arena,
    ) -> Result<Datum<'out>, SqlError> {
        let width = self.width.ok_or_else(inactive)?;
        if index >= width {
            return Err(layout_changed());
        }
        let copied = output
            .alloc_slice_copy(&self.bytes[..self.length])
            .map_err(|_| full())?;
        decode_projected_col_record(copied, index, output)
    }

    fn restore<'out>(
        &self,
        values: &mut [Datum<'out>],
        output: &'out Arena,
    ) -> Result<(), SqlError> {
        let width = self.width.ok_or_else(inactive)?;
        if width != values.len() {
            return Err(layout_changed());
        }
        if width == 0 {
            return Ok(());
        }
        let copied = output
            .alloc_slice_copy(&self.bytes[..self.length])
            .map_err(|_| full())?;
        let decoded = super::projected::decode_projected_row_into(copied, output, values)?;
        debug_assert_eq!(decoded, width);
        Ok(())
    }

    pub(super) fn clear(&mut self) {
        self.bytes.clear();
        self.length = 0;
        self.width = None;
    }
}

/// Active slots may borrow workspace payloads; suspension saves those bytes and
/// clears every reference while keeping the original slot reservation.
pub(super) struct LocalValues<'a> {
    slots: MutableSlice<Datum<'a>>,
    saved: SavedValues,
}

impl<'a> LocalValues<'a> {
    pub(super) fn row_pool(&self) -> &std::sync::Arc<LeasePool> {
        self.saved.bytes.pool()
    }

    pub(super) fn new(
        controls: &std::sync::Arc<LeasePool>,
        rows: &std::sync::Arc<LeasePool>,
        count: usize,
    ) -> Result<Self, SqlError> {
        Ok(Self {
            slots: allocate_plpgsql_local_values(controls, count)?,
            saved: SavedValues::new(rows)?,
        })
    }

    pub(super) fn capture(&mut self) -> Result<(), SqlError> {
        self.saved.replace(&self.slots)
    }

    pub(super) fn restore(&mut self, output: &'a Arena) -> Result<(), SqlError> {
        self.saved.restore(&mut self.slots, output)
    }

    pub(super) fn discard_saved(&mut self) {
        self.saved.clear();
    }

    pub(super) fn suspend(self) -> LocalValues<'static> {
        assert_eq!(self.saved.width, Some(self.slots.len()));
        LocalValues {
            slots: self.slots.map_in_place(|_| Datum::Null),
            saved: self.saved,
        }
    }

    pub(super) fn suspend_unused(mut self) -> LocalValues<'static> {
        assert!(self.slots.iter().all(Datum::is_null));
        self.saved.clear();
        LocalValues {
            slots: self.slots.map_in_place(|_| Datum::Null),
            saved: self.saved,
        }
    }
}

impl LocalValues<'static> {
    pub(super) fn rebind_saved<'out>(self) -> LocalValues<'out> {
        assert_eq!(self.saved.width, Some(self.slots.len()));
        LocalValues {
            slots: self.slots.map_in_place(|_| Datum::Null),
            saved: self.saved,
        }
    }

    pub(super) fn rebind_unused<'out>(self) -> LocalValues<'out> {
        assert!(self.saved.width.is_none());
        LocalValues {
            slots: self.slots.map_in_place(|_| Datum::Null),
            saved: self.saved,
        }
    }
}

impl<'a> core::ops::Deref for LocalValues<'a> {
    type Target = [Datum<'a>];
    fn deref(&self) -> &Self::Target {
        &self.slots
    }
}

impl core::ops::DerefMut for LocalValues<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.slots
    }
}

fn layout_changed() -> SqlError {
    sql_err!(
        sqlstate::INTERNAL_ERROR,
        "PL/pgSQL saved value layout changed"
    )
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
