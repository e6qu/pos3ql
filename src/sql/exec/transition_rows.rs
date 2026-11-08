//! Transition tables have one collection phase and an immutable published image.

use super::*;
use crate::mem::lease_pool::SharedSlice;
use std::sync::Arc;

/// All rows produced by a statement for one operation. AFTER ROW and AFTER
/// STATEMENT triggers bind only the transition aliases they declared.
#[derive(Clone, Copy)]
pub(super) struct TransitionRows<'a> {
    pub(super) old: &'a [&'a [u8]],
    pub(super) new: &'a [&'a [u8]],
}

/// A mixed command retains each operation's completed tables while its row
/// events drain in their original order.
#[derive(Clone, Copy)]
pub(super) struct TransitionRowSets<'a> {
    pub(super) inserted: Option<TransitionRows<'a>>,
    pub(super) updated: Option<TransitionRows<'a>>,
    pub(super) deleted: Option<TransitionRows<'a>>,
}

impl<'a> TransitionRowSets<'a> {
    pub(super) const EMPTY: Self = Self {
        inserted: None,
        updated: None,
        deleted: None,
    };

    pub(super) fn for_event(self, event: u8) -> Option<TransitionRows<'a>> {
        match event {
            TriggerEvents::INSERT => self.inserted,
            TriggerEvents::UPDATE => self.updated,
            TriggerEvents::DELETE => self.deleted,
            TriggerEvents::TRUNCATE => None,
            _ => unreachable!("trigger event validated at its parser boundary"),
        }
    }
}

/// Pre-sized transition row slots. Each DML path derives capacity from its
/// candidate count, so capture cannot grow a pool; encoded rows live in the
/// fixed statement arena and exhaustion is a named error.
pub(super) struct TransitionCapture<'a> {
    old: &'a mut [&'a [u8]],
    old_len: usize,
    new: &'a mut [&'a [u8]],
    new_len: usize,
}

impl<'a> TransitionCapture<'a> {
    pub(super) fn new(
        arena: &'a Arena,
        old_capacity: usize,
        new_capacity: usize,
    ) -> Result<Self, SqlError> {
        let old = arena
            .alloc_slice_with(old_capacity, |_| &[][..])
            .map_err(|_| super::super::query::arena_full_pub())?;
        let new = arena
            .alloc_slice_with(new_capacity, |_| &[][..])
            .map_err(|_| super::super::query::arena_full_pub())?;
        Ok(Self {
            old,
            old_len: 0,
            new,
            new_len: 0,
        })
    }

    pub(super) fn push_old(
        &mut self,
        values: &[Datum<'_>],
        arena: &'a Arena,
    ) -> Result<(), SqlError> {
        if self.old_len == self.old.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "OLD TABLE exceeds its pre-sized statement capture"
            ));
        }
        let row = encode_projected_pub(values, arena)?;
        self.old[self.old_len] = row;
        self.old_len += 1;
        Ok(())
    }

    pub(super) fn push_new(
        &mut self,
        values: &[Datum<'_>],
        arena: &'a Arena,
    ) -> Result<(), SqlError> {
        if self.new_len == self.new.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "NEW TABLE exceeds its pre-sized statement capture"
            ));
        }
        let row = encode_projected_pub(values, arena)?;
        self.new[self.new_len] = row;
        self.new_len += 1;
        Ok(())
    }

    pub(super) fn into_rows(self) -> TransitionRows<'a> {
        // Consuming the writer prevents later mutation of a published row set.
        TransitionRows {
            old: &self.old[..self.old_len],
            new: &self.new[..self.new_len],
        }
    }
}

/// A paused write owns both row sets without retaining its collection arena.
/// Resumption allocates only row directories; payloads borrow this immutable owner.
#[derive(Clone)]
pub(super) struct SavedTransitionRows {
    bytes: SharedSlice<u8>,
    old_end: usize,
    old_count: usize,
    row_count: usize,
}

impl SavedTransitionRows {
    pub(super) fn capture(
        rows: TransitionRows<'_>,
        pool: &Arc<LeasePool>,
    ) -> Result<Self, SqlError> {
        let row_count = rows.old.len().checked_add(rows.new.len()).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "trigger transition row count exceeds addressable work memory"
            )
        })?;
        let old_end = projected::projected_rows_len(rows.old)?;
        let length = old_end
            .checked_add(projected::projected_rows_len(rows.new)?)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "trigger transition rows exceed addressable work memory"
                )
            })?;
        let full = |error| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "trigger transition rows: {}",
                error
            )
        };
        let mut bytes = pool.lease::<u8>(length).map_err(full)?;
        const ZEROES: [u8; 4096] = [0; 4096];
        while bytes.len() < length {
            super::super::query::check_timeout()?;
            let count = (length - bytes.len()).min(ZEROES.len());
            bytes.extend_from_slice(&ZEROES[..count]).map_err(full)?;
        }
        projected::pack_projected_rows_into(rows.old, &mut bytes[..old_end])?;
        projected::pack_projected_rows_into(rows.new, &mut bytes[old_end..])?;
        Ok(Self {
            bytes: bytes.freeze(),
            old_end,
            old_count: rows.old.len(),
            row_count,
        })
    }

    pub(super) fn restore<'out>(
        &'out self,
        arena: &'out Arena,
    ) -> Result<TransitionRows<'out>, SqlError> {
        let mut position = 8;
        // Admit both directories at once. No rollback of scratch is needed,
        // including when its parent currently lends a temporary region.
        let rows = arena
            .alloc_slice_with(self.row_count, |index| {
                if index == self.old_count {
                    position = self.old_end + 8;
                }
                let length = usize::try_from(u64::from_le_bytes(
                    self.bytes[position..position + 8]
                        .try_into()
                        .expect("captured row length header"),
                ))
                .expect("captured row length fits the original address space");
                position += 8;
                let row = &self.bytes[position..position + length];
                position += length;
                row
            })
            .map_err(|_| super::super::query::arena_full_pub())?;
        let (old, new) = rows.split_at(self.old_count);
        Ok(TransitionRows { old, new })
    }
}

#[cfg(test)]
mod ownership_tests;
