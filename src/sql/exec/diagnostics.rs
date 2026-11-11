//! Handler buffers own their bytes; borrowed views cannot outlive or reset them.

use super::*;

#[derive(Clone, Copy)]
pub(super) enum Field {
    Sqlstate,
    Message,
    Detail,
    Hint,
    Context,
    Schema,
    Table,
    Column,
    Datatype,
    Constraint,
    InternalQuery,
    InternalPosition,
}

const FIELDS: usize = 12;

/// The object fields in [`crate::sql::eval::ErrorObjects`] order.
const OBJECT_FIELDS: [Field; 5] = [
    Field::Schema,
    Field::Table,
    Field::Column,
    Field::Datatype,
    Field::Constraint,
];

impl Field {
    const fn index(self) -> usize {
        match self {
            Self::Sqlstate => 0,
            Self::Message => 1,
            Self::Detail => 2,
            Self::Hint => 3,
            Self::Context => 4,
            Self::Schema => 5,
            Self::Table => 6,
            Self::Column => 7,
            Self::Datatype => 8,
            Self::Constraint => 9,
            Self::InternalQuery => 10,
            Self::InternalPosition => 11,
        }
    }
}

pub(super) fn objects_mut(
    objects: &mut crate::sql::eval::ErrorObjects,
) -> [&mut Option<StackStr<{ crate::sql::eval::MAX_ERROR_OBJECT_BYTES }>>; 5] {
    [
        &mut objects.schema,
        &mut objects.table,
        &mut objects.column,
        &mut objects.datatype,
        &mut objects.constraint,
    ]
}

fn object_texts(objects: &crate::sql::eval::ErrorObjects) -> [Option<&str>; 5] {
    [
        &objects.schema,
        &objects.table,
        &objects.column,
        &objects.datatype,
        &objects.constraint,
    ]
    .map(|value| value.as_ref().map(StackStr::as_str))
}

#[derive(Clone, Copy)]
struct Span {
    start: usize,
    len: usize,
}

/// Cleanup can call SQL or wait on storage before the original error reaches
/// its handler. Its detail and hint must leave the worker-local mailbox first.
pub(super) struct PendingError {
    pub(super) error: SqlError,
    diagnostic: Result<Option<PendingDiagnostic>, SqlError>,
}

struct PendingDiagnostic {
    bytes: MutableSlice<u8>,
    /// The complete message of an error whose inline message is its start.
    message: Option<Span>,
    detail: Option<Span>,
    hint: Option<Span>,
    objects: [Option<Span>; 5],
    context: Option<Span>,
    internal_query: Option<Span>,
    internal_position: Option<usize>,
}

impl PendingDiagnostic {
    fn text(&self, span: Option<Span>) -> Option<&str> {
        span.map(|span| {
            core::str::from_utf8(&self.bytes[span.start..span.start + span.len])
                .expect("pending diagnostics copy complete UTF-8 strings")
        })
    }
}

impl PendingError {
    pub(super) fn capture(error: SqlError, pool: &std::sync::Arc<LeasePool>) -> Self {
        let diagnostic = Self::capture_diagnostic(&error, pool);
        Self { error, diagnostic }
    }

    fn capture_diagnostic(
        error: &SqlError,
        pool: &std::sync::Arc<LeasePool>,
    ) -> Result<Option<PendingDiagnostic>, SqlError> {
        let diagnostic = crate::sql::eval::take_diagnostic();
        crate::sql::error_position::peek_full_message(error, |message| {
            crate::sql::error_context::take(error, |reported| {
                if diagnostic.is_none() && reported.is_none() && message.is_none() {
                    return Ok(None);
                }
                let detail = diagnostic
                    .as_ref()
                    .and_then(|d| d.detail.as_ref().map(StackStr::as_str));
                let hint = diagnostic
                    .as_ref()
                    .and_then(|d| d.hint.as_ref().map(StackStr::as_str));
                let objects = diagnostic
                    .as_ref()
                    .map_or([None; 5], |d| object_texts(&d.objects));
                let context = reported.as_ref().map(|reported| reported.context);
                let internal = reported.as_ref().and_then(|reported| reported.internal);
                let length = message.map_or(0, str::len)
                    + detail.map_or(0, str::len)
                    + hint.map_or(0, str::len)
                    + objects
                        .iter()
                        .map(|text| text.map_or(0, str::len))
                        .sum::<usize>()
                    + context.map_or(0, str::len)
                    + internal.map_or(0, |(query, _)| query.len());
                let mut bytes = pool.lease(length).map_err(exhausted)?;
                let mut copy = |text: Option<&str>| -> Result<Option<Span>, SqlError> {
                    text.map(|text| {
                        let span = Span {
                            start: bytes.len(),
                            len: text.len(),
                        };
                        bytes
                            .extend_from_slice(text.as_bytes())
                            .map_err(exhausted)?;
                        Ok(span)
                    })
                    .transpose()
                };
                let message = copy(message)?;
                let detail = copy(detail)?;
                let hint = copy(hint)?;
                let mut object_spans = [None; 5];
                for (span, text) in object_spans.iter_mut().zip(objects) {
                    *span = copy(text)?;
                }
                let context = copy(context)?;
                let internal_query = copy(internal.map(|(query, _)| query))?;
                Ok(Some(PendingDiagnostic {
                    bytes,
                    message,
                    detail,
                    hint,
                    objects: object_spans,
                    context,
                    internal_query,
                    internal_position: internal.map(|(_, position)| position),
                }))
            })
        })
    }

    pub(super) fn catch(self, buffers: &mut DiagnosticBuffers) -> Result<(), SqlError> {
        // Select and unwind the original error's block first. If its image
        // could not be saved, fail outside that block rather than entering its
        // handler with an unrelated capacity error.
        let diagnostic = self.diagnostic?;
        let message = diagnostic.as_ref().and_then(|d| d.text(d.message));
        let detail = diagnostic.as_ref().and_then(|d| d.text(d.detail));
        let hint = diagnostic.as_ref().and_then(|d| d.text(d.hint));
        let objects = diagnostic
            .as_ref()
            .map_or([None; 5], |d| d.objects.map(|span| d.text(span)));
        let context = diagnostic.as_ref().and_then(|d| d.text(d.context));
        let internal = diagnostic
            .as_ref()
            .and_then(|d| Some((d.text(d.internal_query)?, d.internal_position?)));
        let mut position = StackStr::<20>::new();
        if let Some((_, at)) = internal {
            let _ = core::fmt::Write::write_fmt(&mut position, format_args!("{at}"));
        }
        buffers.capture_fields(
            &self.error,
            message,
            detail,
            hint,
            context.unwrap_or(""),
            objects,
            internal.map(|(query, _)| (query, position.as_str())),
        )
    }

    pub(super) fn publish(self) -> SqlError {
        // A returned error becomes the next mailbox owner. Discard diagnostics
        // left by cleanup, including when the original error had none.
        let _ = crate::sql::eval::take_diagnostic();
        let diagnostic = match self.diagnostic {
            Ok(diagnostic) => diagnostic,
            Err(capacity_error) => return capacity_error,
        };
        if let Some(diagnostic) = diagnostic {
            if let Some(message) = diagnostic.text(diagnostic.message) {
                crate::sql::error_position::keep_full_message(&self.error, message);
            }
            let mut objects = crate::sql::eval::ErrorObjects::default();
            for (field, span) in objects_mut(&mut objects)
                .into_iter()
                .zip(diagnostic.objects)
            {
                *field = diagnostic.text(span).map(StackStr::from_str);
            }
            let detail = diagnostic.text(diagnostic.detail).map(StackStr::from_str);
            let hint = diagnostic.text(diagnostic.hint).map(StackStr::from_str);
            if detail.is_some() || hint.is_some() || !objects.is_empty() {
                crate::sql::eval::stash_full_diagnostic(crate::sql::eval::Diagnostic {
                    detail,
                    hint,
                    objects,
                });
            }
            if let Some(context) = diagnostic.text(diagnostic.context) {
                let internal = diagnostic
                    .text(diagnostic.internal_query)
                    .zip(diagnostic.internal_position);
                crate::sql::error_context::restore(&self.error, context, internal);
            }
        }
        self.error
    }
}

#[derive(Clone, Copy)]
pub(super) struct CaughtDiagnostic<'text> {
    pub(super) sqlstate: crate::sql::eval::SqlState,
    bytes: &'text [u8],
    spans: [Option<Span>; FIELDS],
}

impl<'text> CaughtDiagnostic<'text> {
    pub(super) fn field(self, field: Field) -> Option<&'text str> {
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

    /// The caught error's context and internal query, for `RAISE;`.
    pub(super) fn restore_context(self, error: &SqlError) {
        let Some(context) = self.field(Field::Context) else {
            return;
        };
        let internal = self.field(Field::InternalQuery).zip(
            self.field(Field::InternalPosition)
                .and_then(|position| position.parse().ok()),
        );
        crate::sql::error_context::restore(error, context, internal);
    }

    /// The caught error's object fields, for `RAISE;`.
    pub(super) fn objects(self) -> Result<crate::sql::eval::ErrorObjects, SqlError> {
        let mut objects = crate::sql::eval::ErrorObjects::default();
        for (target, field) in objects_mut(&mut objects).into_iter().zip(OBJECT_FIELDS) {
            *target = self.stack(field)?;
        }
        Ok(objects)
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
    spans: [Option<Span>; FIELDS],
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
                    spans: [None; FIELDS],
                })
                .map_err(exhausted)?;
        }
        Ok(Self { slots, active: 0 })
    }

    #[cfg(test)]
    pub(super) fn capture(
        &mut self,
        error: &SqlError,
        diagnostic: Option<&crate::sql::eval::Diagnostic>,
        context: &str,
    ) -> Result<(), SqlError> {
        self.capture_fields(
            error,
            None,
            diagnostic.and_then(|value| value.detail.as_ref().map(StackStr::as_str)),
            diagnostic.and_then(|value| value.hint.as_ref().map(StackStr::as_str)),
            context,
            diagnostic.map_or([None; 5], |value| object_texts(&value.objects)),
            None,
        )
    }

    /// `message` is the error's complete message when its inline one is only
    /// its start.
    #[expect(
        clippy::too_many_arguments,
        reason = "a caught error's fields are copied together into one image"
    )]
    fn capture_fields(
        &mut self,
        error: &SqlError,
        message: Option<&str>,
        detail: Option<&str>,
        hint: Option<&str>,
        context: &str,
        objects: [Option<&str>; 5],
        internal: Option<(&str, &str)>,
    ) -> Result<(), SqlError> {
        let slot = self.slots.get_mut(self.active).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "PL/pgSQL diagnostic handler depth exceeds reserved capacity"
            )
        })?;
        let fields = [
            Some(error.sqlstate.as_str()),
            Some(message.unwrap_or(error.message.as_str())),
            detail,
            hint,
            Some(context),
            objects[0],
            objects[1],
            objects[2],
            objects[3],
            objects[4],
            internal.map(|(query, _)| query),
            internal.map(|(_, position)| position),
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
        let mut spans = [None; FIELDS];
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
        slot.spans = [None; FIELDS];
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
