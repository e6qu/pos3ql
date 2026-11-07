//! Handler buffers own their bytes; borrowed views cannot outlive or reset them.

use super::*;

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
pub(super) struct CaughtDiagnostic<'text> {
    pub(super) sqlstate: crate::sql::eval::SqlState,
    bytes: &'text [u8],
    spans: [Option<Span>; 5],
}

impl<'text> CaughtDiagnostic<'text> {
    fn field(self, field: Field) -> Option<&'text str> {
        let span = self.spans[field.index()]?;
        Some(
            core::str::from_utf8(&self.bytes[span.start..span.start + span.len])
                .expect("diagnostic fields copy complete UTF-8 strings"),
        )
    }

    pub(super) fn text(self, field: Field, destination: &Arena) -> Result<Option<&str>, SqlError> {
        self.field(field)
            .map(|text| destination.alloc_str(text).map_err(|_| memory_full()))
            .transpose()
    }

    pub(super) fn stack<const N: usize>(
        self,
        field: Field,
    ) -> Result<Option<StackStr<N>>, SqlError> {
        self.field(field)
            .map(|text| {
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
            .transpose()
    }

    pub(super) fn variables(
        self,
        destination: &Arena,
    ) -> Result<TriggerExceptionVariables<'_>, SqlError> {
        Ok(TriggerExceptionVariables {
            sqlstate_text: self
                .text(Field::Sqlstate, destination)?
                .expect("caught error has SQLSTATE"),
            message: self
                .text(Field::Message, destination)?
                .expect("caught error has a message"),
        })
    }
}

struct Buffer {
    bytes: MutableSlice<u8>,
    sqlstate: Option<crate::sql::eval::SqlState>,
    length: usize,
    spans: [Option<Span>; 5],
}

pub(super) struct DiagnosticBuffers {
    slots: MutableSlice<Buffer>,
    active: usize,
}

impl DiagnosticBuffers {
    pub(super) fn new(pool: &std::sync::Arc<LeasePool>, count: usize) -> Result<Self, SqlError> {
        let mut slots = pool.lease(count).map_err(exhausted)?;
        for _ in 0..count {
            slots
                .push_reserved(Buffer {
                    bytes: pool.lease(0).map_err(exhausted)?,
                    sqlstate: None,
                    length: 0,
                    spans: [None; 5],
                })
                .map_err(exhausted)?;
        }
        Ok(Self { slots, active: 0 })
    }

    pub(super) fn capture(
        &mut self,
        error: &SqlError,
        diagnostic: Option<&crate::sql::eval::Diagnostic>,
        context: &str,
    ) -> Result<(), SqlError> {
        let slot = self.slots.get_mut(self.active).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "PL/pgSQL diagnostic handler depth exceeds reserved capacity"
            )
        })?;
        let fields = [
            Some(error.sqlstate.as_str()),
            Some(error.message.as_str()),
            diagnostic.and_then(|value| value.detail.as_ref().map(StackStr::as_str)),
            diagnostic.and_then(|value| value.hint.as_ref().map(StackStr::as_str)),
            Some(context),
        ];
        let length = fields.iter().try_fold(0usize, |total, value| {
            total
                .checked_add(value.map_or(0, str::len))
                .ok_or_else(memory_full)
        })?;
        let capacity = length
            .checked_next_power_of_two()
            .ok_or_else(memory_full)?
            .min(slot.bytes.max_entries())
            .max(length);
        slot.bytes.reserve_exact(capacity).map_err(exhausted)?;
        const ZEROES: [u8; 4096] = [0; 4096];
        while slot.bytes.len() < length {
            super::super::query::check_timeout()?;
            let count = (length - slot.bytes.len()).min(ZEROES.len());
            slot.bytes
                .extend_from_slice(&ZEROES[..count])
                .map_err(exhausted)?;
        }
        // Sizing and reservation finish before any published metadata changes.
        let mut spans = [None; 5];
        let mut position = 0;
        for (span, field) in spans.iter_mut().zip(fields) {
            if let Some(field) = field {
                slot.bytes[position..position + field.len()].copy_from_slice(field.as_bytes());
                *span = Some(Span {
                    start: position,
                    len: field.len(),
                });
                position += field.len();
            }
        }
        slot.spans = spans;
        slot.length = length;
        slot.sqlstate = Some(error.sqlstate);
        self.active += 1;
        Ok(())
    }

    pub(super) fn current(&self) -> Option<CaughtDiagnostic<'_>> {
        let slot = self
            .slots
            .get(self.active.checked_sub(1)?)
            .expect("active diagnostic slot");
        Some(CaughtDiagnostic {
            sqlstate: slot.sqlstate.expect("active diagnostic is published"),
            bytes: &slot.bytes[..slot.length],
            spans: slot.spans,
        })
    }

    pub(super) fn release(&mut self) {
        self.active = self.active.checked_sub(1).expect("active diagnostic scope");
        let slot = &mut self.slots[self.active];
        slot.sqlstate = None;
        slot.length = 0;
        slot.spans = [None; 5];
    }
}

fn exhausted(error: crate::mem::lease_pool::LeaseError) -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "PL/pgSQL diagnostics: {}",
        error
    )
}

fn memory_full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "PL/pgSQL diagnostic copy exceeds statement memory"
    )
}

#[cfg(test)]
mod ownership_tests;
