//! Caught errors reuse checked storage; exposed variables own independent text.

use super::*;
use crate::mem::arena::ArenaSliceHandle;

#[derive(Clone, Copy)]
pub(super) enum Field {
    Sqlstate,
    Message,
    Detail,
    Hint,
    Context,
}

impl Field {
    const fn index(self) -> usize {
        match self {
            Self::Sqlstate => 0,
            Self::Message => 1,
            Self::Detail => 2,
            Self::Hint => 3,
            Self::Context => 4,
        }
    }
}

#[derive(Clone, Copy)]
struct Span {
    start: usize,
    len: usize,
}

#[derive(Clone, Copy)]
pub(super) struct CaughtDiagnostic<'a> {
    pub(super) sqlstate: crate::sql::eval::SqlState,
    arena: &'a Arena,
    bytes: ArenaSliceHandle<u8>,
    generation: u64,
    slot: usize,
    spans: [Option<Span>; 5],
}

impl CaughtDiagnostic<'_> {
    fn read<R>(
        self,
        field: Field,
        copy: impl for<'text> FnOnce(&'text str) -> Result<R, SqlError>,
    ) -> Result<Option<R>, SqlError> {
        let bytes = self
            .arena
            .retained_slice_mut(self.bytes)
            .map_err(|_| inactive())?;
        let generation = u64::from_le_bytes(bytes[..8].try_into().expect("diagnostic generation"));
        if generation != self.generation {
            return Err(inactive());
        }
        let Some(span) = self.spans[field.index()] else {
            return Ok(None);
        };
        let text = bytes
            .get(span.start..span.start + span.len)
            .and_then(|bytes| core::str::from_utf8(bytes).ok())
            .ok_or_else(inactive)?;
        copy(text).map(Some)
    }

    pub(super) fn text<'out>(
        self,
        field: Field,
        arena: &'out Arena,
    ) -> Result<Option<&'out str>, SqlError> {
        self.read(field, |text| {
            arena.alloc_str(text).map_err(|_| memory_full())
        })
    }

    pub(super) fn stack<const N: usize>(
        self,
        field: Field,
    ) -> Result<Option<StackStr<N>>, SqlError> {
        self.read(field, |text| {
            let result = StackStr::from_str(text);
            if result.is_truncated() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "PL/pgSQL diagnostic exceeds its {} byte destination",
                    N
                ));
            }
            Ok(result)
        })
    }

    pub(super) fn variables<'out>(
        self,
        arena: &'out Arena,
    ) -> Result<TriggerExceptionVariables<'out>, SqlError> {
        Ok(TriggerExceptionVariables {
            sqlstate_text: self
                .text(Field::Sqlstate, arena)?
                .expect("caught error has SQLSTATE"),
            message: self
                .text(Field::Message, arena)?
                .expect("caught error has a message"),
        })
    }
}

#[derive(Clone, Copy)]
struct Buffer {
    bytes: Option<ArenaSliceHandle<u8>>,
    capacity: usize,
    generation: u64,
}

pub(super) struct DiagnosticBuffers<'a> {
    arena: &'a Arena,
    slots: &'a mut [Buffer],
    active: usize,
}

impl<'a> DiagnosticBuffers<'a> {
    pub(super) fn new(arena: &'a Arena, count: usize) -> Result<Self, SqlError> {
        Ok(Self {
            arena,
            slots: arena
                .alloc_slice_with(count, |_| Buffer {
                    bytes: None,
                    capacity: 0,
                    generation: 0,
                })
                .map_err(|_| memory_full())?,
            active: 0,
        })
    }

    pub(super) fn capture(
        &mut self,
        error: &SqlError,
        diagnostic: Option<&crate::sql::eval::Diagnostic>,
        context: &str,
    ) -> Result<CaughtDiagnostic<'a>, SqlError> {
        let slot_index = self.active;
        let slot = self.slots.get_mut(slot_index).ok_or_else(memory_full)?;
        let generation = slot.generation.checked_add(1).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "PL/pgSQL diagnostic generation exhausted"
            )
        })?;
        let fields = [
            Some(error.sqlstate.as_str()),
            Some(error.message.as_str()),
            diagnostic.and_then(|value| value.detail.as_ref().map(StackStr::as_str)),
            diagnostic.and_then(|value| value.hint.as_ref().map(StackStr::as_str)),
            Some(context),
        ];
        let length = fields.iter().try_fold(8usize, |total, value| {
            total
                .checked_add(value.map_or(0, str::len))
                .ok_or_else(memory_full)
        })?;
        if length > slot.capacity {
            // Geometric payload growth bounds storage by the largest error
            // seen at this depth, rather than by the number of catches.
            let capacity = length.checked_next_power_of_two().ok_or_else(memory_full)?;
            let bytes = self
                .arena
                .retain_slice_with(capacity, |_| 0u8)
                .map_err(|_| memory_full())?;
            slot.bytes = Some(bytes);
            slot.capacity = capacity;
        }
        let handle = slot.bytes.expect("nonempty diagnostic buffer");
        let mut bytes = self
            .arena
            .retained_slice_mut(handle)
            .map_err(|_| inactive())?;
        bytes[..8].fill(0);
        let mut spans = [None; 5];
        let mut position = 8;
        for (span, field) in spans.iter_mut().zip(fields) {
            if let Some(field) = field {
                bytes[position..position + field.len()].copy_from_slice(field.as_bytes());
                *span = Some(Span {
                    start: position,
                    len: field.len(),
                });
                position += field.len();
            }
        }
        bytes[..8].copy_from_slice(&generation.to_le_bytes());
        slot.generation = generation;
        self.active += 1;
        Ok(CaughtDiagnostic {
            sqlstate: error.sqlstate,
            arena: self.arena,
            bytes: handle,
            generation,
            slot: slot_index,
            spans,
        })
    }

    pub(super) fn release(&mut self, diagnostic: CaughtDiagnostic<'_>) {
        assert_eq!(self.active, diagnostic.slot + 1, "diagnostic scope order");
        self.arena
            .retained_slice_mut(diagnostic.bytes)
            .expect("active diagnostic handle")[..8]
            .fill(0);
        self.active -= 1;
    }
}

fn memory_full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "PL/pgSQL diagnostic workspace exceeds statement memory"
    )
}

fn inactive() -> SqlError {
    sql_err!(
        sqlstate::INTERNAL_ERROR,
        "PL/pgSQL diagnostic reference is no longer active"
    )
}

#[cfg(test)]
mod ownership_tests;
