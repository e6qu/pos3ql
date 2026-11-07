//! Set-function results own row bytes and actual output metadata in shared work memory.

use super::*;
use crate::mem::lease_pool::SharedSlice;
use std::sync::Arc;

enum Contract {
    Scalar(crate::storage::RoutineResult),
    Columns(SharedSlice<RoutineArgumentDef>),
}

pub(super) struct PlpgsqlSetResult {
    contract: Contract,
    bytes: MutableSlice<u8>,
    ends: MutableSlice<usize>,
}

impl PlpgsqlSetResult {
    pub(super) fn for_routine(
        routine: &crate::storage::RoutineDef,
        pool: &Arc<LeasePool>,
    ) -> Result<Self, SqlError> {
        let contract = match routine.kind {
            crate::storage::RoutineKind::SetFunction { result } => Contract::Scalar(result),
            crate::storage::RoutineKind::RecordFunction { .. }
            | crate::storage::RoutineKind::TableFunction => Contract::Columns(
                pool.copy_slice(&routine.result_columns[..routine.result_column_count])
                    .map_err(full)?,
            ),
            _ => unreachable!("set result requires a set-returning routine"),
        };
        Ok(Self {
            contract,
            bytes: pool.lease(0).map_err(full)?,
            ends: pool.lease(0).map_err(full)?,
        })
    }

    pub(super) fn append(&mut self, values: &[Datum<'_>], arena: &Arena) -> Result<(), SqlError> {
        // Conversion scratch cannot become part of the retained result.
        arena
            .with_remaining_region(|scratch| {
                let width = match &self.contract {
                    Contract::Scalar(_) => {
                        if values.len() != 1 {
                            return Err(sql_err!(
                                sqlstate::DATATYPE_MISMATCH,
                                "RETURN NEXT must provide one column"
                            ));
                        }
                        1
                    }
                    Contract::Columns(columns) => {
                        if values.len() != columns.len() {
                            return Err(sql_err!(
                                sqlstate::DATATYPE_MISMATCH,
                                "RETURN NEXT must provide {} column{}",
                                columns.len(),
                                if columns.len() == 1 { "" } else { "s" }
                            ));
                        }
                        columns.len()
                    }
                };
                let cast = scratch
                    .alloc_slice_with(width, |_| Datum::Null)
                    .map_err(|_| super::super::query::arena_full_pub())?;
                match &self.contract {
                    Contract::Scalar(result) => {
                        cast[0] = cast_to(
                            detach_routine_datum(values[0], scratch)?,
                            result.ctype,
                            scratch,
                        )?;
                    }
                    Contract::Columns(columns) => {
                        for (index, column) in columns.iter().enumerate() {
                            cast[index] = cast_to(
                                detach_routine_datum(values[index], scratch)?,
                                column.ctype,
                                scratch,
                            )?;
                        }
                    }
                }
                let cast = &cast[..width];
                let length = projected_row_len(cast)?;
                // Only row ends publish bytes. A failed or cancelled append may
                // leave an initialized suffix, which the next append overwrites.
                let start = match self.ends.last() {
                    Some(end) => *end,
                    None => 0,
                };
                let end = start.checked_add(length).ok_or_else(too_large)?;
                let row_count = self.ends.len().checked_add(1).ok_or_else(too_large)?;
                self.ends.reserve_exact(row_count).map_err(full)?;
                self.bytes.reserve_exact(end).map_err(full)?;
                const ZEROES: [u8; 4096] = [0; 4096];
                while self.bytes.len() < end {
                    super::super::query::check_timeout()?;
                    let count = (end - self.bytes.len()).min(ZEROES.len());
                    self.bytes
                        .extend_from_slice(&ZEROES[..count])
                        .map_err(full)?;
                }
                encode_projected_into(cast, &mut self.bytes[start..end])?;
                self.ends.push_reserved(end).map_err(full)?;
                Ok(())
            })
            .map_err(|_| super::super::query::arena_full_pub())?
    }

    pub(super) fn rows<'a>(&self, arena: &'a Arena) -> Result<&'a [&'a [u8]], SqlError> {
        let rows = arena
            .alloc_slice_with(self.ends.len(), |_| &[][..])
            .map_err(|_| super::super::query::arena_full_pub())?;
        let mut start = 0;
        for (row, end) in rows.iter_mut().zip(self.ends.iter().copied()) {
            *row = arena
                .alloc_slice_copy(&self.bytes[start..end])
                .map_err(|_| super::super::query::arena_full_pub())?;
            start = end;
        }
        Ok(rows)
    }
}

fn full(error: crate::mem::lease_pool::LeaseError) -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "PL/pgSQL set results: {}",
        error
    )
}

fn too_large() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "PL/pgSQL set results exceed addressable work memory"
    )
}

#[cfg(test)]
mod ownership_tests;
