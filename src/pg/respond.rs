//! Backend message construction: the typed layer between the engine and
//! the raw send buffer.

use crate::mem::arena::{Arena, ArenaFull};
use crate::mem::buffer::FixedBuf;
use crate::sql::ast::ExplainSerialize;
use crate::sql::types::{ColDesc, Datum};
use crate::stack_format;

use std::sync::Arc;

use crate::mem::lease_pool::LeasePool;

use super::overflow::{OutputClass, OutputMark, OverflowExhaustion, ResultOverflow};
use super::wire::{self, MsgOut, WireFull};

/// Maximum result columns whose per-column format we track (matches the
/// projection limit).
pub const MAX_RESULT_COLS: usize = crate::sql::exec::MAX_PROJ;

/// Per-column result wire format requested by Bind: text (`false`) or binary
/// (`true`). Encodes PostgreSQL's three cases: no codes (all text), one code
/// (applies to every column), or one code per column.
#[derive(Clone, Copy)]
pub struct ResultFmt {
    codes: [bool; MAX_RESULT_COLS],
    n: u16,
}

impl ResultFmt {
    pub const ALL_TEXT: Self = Self {
        codes: [false; MAX_RESULT_COLS],
        n: 0,
    };

    pub const ALL_BINARY: Self = Self {
        codes: [true; MAX_RESULT_COLS],
        n: 1,
    };

    pub fn new(codes: [bool; MAX_RESULT_COLS], n: u16) -> Self {
        Self { codes, n }
    }

    /// PostgreSQL permits no result formats, one format for every column, or
    /// exactly one format per result column. Bind validates the third case
    /// before a value can reach this emitter.
    pub(crate) fn matches_column_count(&self, columns: usize) -> bool {
        self.n <= 1 || self.n as usize == columns
    }

    pub(crate) fn count(&self) -> u16 {
        self.n
    }

    pub(crate) fn any_binary(&self) -> bool {
        match self.n {
            0 => false,
            1 => self.codes[0],
            n => self.codes[..usize::from(n)].iter().any(|code| *code),
        }
    }

    /// Whether column `col` is requested in binary.
    pub(crate) fn is_binary(&self, col: usize) -> bool {
        match self.n {
            0 => false,
            1 => self.codes[0],
            _ => self.codes[col],
        }
    }
}

/// A deterministic emitter of one COPY row's escaped bytes.
pub type CopyRowWriter<'a> = dyn Fn(&mut dyn FnMut(&[u8])) + 'a;

/// The connection transport a responder may write without blocking while a
/// statement runs: none, a raw fd (plaintext), or the TLS session.
pub(crate) enum ClientWriter<'b> {
    None,
    Fd(i32),
    Tls {
        session: &'b mut crate::pg::tls::ServerSession,
        socket: &'b mut std::net::TcpStream,
    },
}

/// Connection output beyond the send buffer. A full send buffer first tries
/// one non-blocking write (when the caller allows output before
/// publication), then queues complete messages in the overflow area; the
/// statement never waits for the client.
pub(crate) struct OutputSink<'b> {
    overflow: &'b mut ResultOverflow,
    spool: Option<Arc<LeasePool>>,
    writer: ClientWriter<'b>,
    /// The transport failed; the connection closes after this statement.
    failed: bool,
    /// Overflow length that triggers the next write attempt.
    next_write_at: usize,
    promotion: Promotion,
    /// Why the current statement's overflow queued instead of streaming.
    spooled: Option<SpoolReason>,
    /// A result stream keeps at most one row beyond `send` and parks; that
    /// row may use the private reserve instead of the shared spool.
    rows_use_reserve: bool,
}

/// Why a result that outgrew the send buffer queued in the result spool
/// instead of resuming as a result stream. The engine decides before the
/// statement runs, except for effects, which only execution reveals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpoolReason {
    /// The query shape cannot resume from a retained read position.
    Shape,
    /// Its retained read position would exceed `result_stream_bytes`.
    Capacity,
    /// The statement started an effect before its output overflowed.
    Effects,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Promotion {
    /// This output never resumes as a stream.
    Disabled,
    /// The protocol allows a stream; the current statement did not arm one.
    Allowed,
    /// The first overflow of a restartable statement requests a stream.
    Armed { mark: OutputMark, epoch: u64 },
    /// The statement's overflow queues; `spooled` records why.
    Declined(SpoolReason),
    /// The statement stopped at its first overflow and must reopen as a stream.
    Requested,
}

impl<'b> OutputSink<'b> {
    pub(crate) fn new(
        overflow: &'b mut ResultOverflow,
        spool: Option<Arc<LeasePool>>,
        writer: ClientWriter<'b>,
    ) -> Self {
        Self {
            overflow,
            spool,
            writer,
            failed: false,
            next_write_at: 0,
            promotion: Promotion::Disabled,
            spooled: None,
            rows_use_reserve: false,
        }
    }

    /// Lets a top-level `SELECT` in this output resume as a result stream.
    pub(crate) fn allow_promotion(mut self) -> Self {
        self.promotion = Promotion::Allowed;
        self
    }

    /// Output of a result stream: rows never need the shared spool.
    pub(crate) fn for_stream(mut self) -> Self {
        self.rows_use_reserve = true;
        self.spool = None;
        self
    }

    /// Writes queued output until the transport would block, refilling `send`
    /// from the overflow at message boundaries.
    fn write_available(&mut self, send: &mut FixedBuf) {
        if matches!(self.writer, ClientWriter::None) || self.failed {
            return;
        }
        loop {
            if send.is_empty() && !self.overflow.refill(send) {
                break;
            }
            let accepted = match &mut self.writer {
                ClientWriter::None => unreachable!("checked above"),
                ClientWriter::Fd(fd) => {
                    let data = send.readable();
                    let n = unsafe { libc::write(*fd, data.as_ptr().cast(), data.len()) };
                    if n > 0 {
                        n as usize
                    } else if n < 0 {
                        match std::io::Error::last_os_error().kind() {
                            std::io::ErrorKind::Interrupted => continue,
                            std::io::ErrorKind::WouldBlock => 0,
                            _ => {
                                self.failed = true;
                                0
                            }
                        }
                    } else {
                        0
                    }
                }
                ClientWriter::Tls { session, socket } => {
                    match session.queue_nonblocking(socket, send.readable()) {
                        Ok(n) => n,
                        Err(_) => {
                            self.failed = true;
                            0
                        }
                    }
                }
            };
            if accepted == 0 {
                break;
            }
            // A sent prefix cannot be rolled back by a statement retry.
            crate::sql::query::protect_statement_effects();
            send.consume(accepted);
            self.overflow.record_written(accepted);
            let _ = self.overflow.refill(send);
        }
        self.next_write_at = self.overflow.len().saturating_add(send.capacity() / 2);
    }
}

pub(crate) enum CursorRowOutput<'b> {
    Wire,
    Save(&'b mut FixedBuf),
    Discard,
}

pub struct Responder<'b> {
    pub buffer: &'b mut FixedBuf,
    /// SQL cursors retain typed rows; FETCH chooses formats and rendering.
    cursor_rows: CursorRowOutput<'b>,
    /// Extended-protocol Execute must not resend RowDescription (the
    /// client got it from Describe).
    suppress_row_description: bool,
    /// Per-column result format requested by Bind.
    formats: ResultFmt,
    /// Type identity from the most recent RowDescription. The executor needs
    /// this even when extended Execute suppresses that message: a NULL value
    /// does not itself retain the result column's catalog type.
    result_type_oids: [i32; MAX_RESULT_COLS],
    result_columns: usize,
    /// Where output goes once `buffer` is full; without one, a full buffer
    /// fails the message with `WireFull`.
    output: Option<OutputSink<'b>>,
    /// Session value-rendering settings (DateStyle, time zone).
    render: crate::sql::guc::RenderContext,
    /// EXPLAIN ANALYZE executes through the ordinary executor but suppresses
    /// its row description, rows, and command tag. Errors still propagate.
    discard_query_output: bool,
    /// Composite utility commands execute ordinary DROP helpers but publish
    /// one command tag of their own.
    suppress_command_complete: bool,
    /// Last complete frame produced by the typed command writer. DDL keeps
    /// its tag in statement memory while end triggers run, preserving notices.
    last_command_completion: Option<(OutputMark, usize)>,
    /// Typed affected-row count for an internally executed DML command. This
    /// never derives state from a serialized command tag.
    affected_rows: Option<u64>,
    discarded_rows: u64,
    discard_serialize: ExplainSerialize,
    serialized_bytes: u64,
    serialization_micros: u64,
}

pub(crate) struct ResultFormatScope<'scope, 'buffer> {
    responder: &'scope mut Responder<'buffer>,
    previous: ResultFmt,
}

impl<'buffer> core::ops::Deref for ResultFormatScope<'_, 'buffer> {
    type Target = Responder<'buffer>;

    fn deref(&self) -> &Self::Target {
        self.responder
    }
}

impl core::ops::DerefMut for ResultFormatScope<'_, '_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.responder
    }
}

impl Drop for ResultFormatScope<'_, '_> {
    fn drop(&mut self) {
        self.responder.formats = self.previous;
    }
}

#[derive(Clone, Copy)]
pub(crate) struct DiscardedOutput {
    pub(crate) rows: u64,
    pub(crate) serialized_bytes: u64,
    pub(crate) serialization_micros: u64,
}

fn display_len(value: impl core::fmt::Display) -> usize {
    struct Counter(usize);
    impl core::fmt::Write for Counter {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            self.0 = self.0.saturating_add(text.len());
            Ok(())
        }
    }
    use core::fmt::Write as _;
    let mut counter = Counter(0);
    let _ = write!(counter, "{value}");
    counter.0
}

/// Streams output-function bytes without making PostgreSQL's internal
/// one-byte `"char"` into a UTF-8 character. Arrays and records recurse so a
/// byte stays exact wherever a client can observe it.
fn write_wire_text(
    value: &Datum,
    render: crate::sql::guc::RenderContext,
    out: &mut dyn FnMut(&[u8]),
) {
    struct Writer<'a>(&'a mut dyn FnMut(&[u8]));
    impl core::fmt::Write for Writer<'_> {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            self.0(text.as_bytes());
            Ok(())
        }
    }
    match value {
        Datum::Char(0) => {}
        Datum::Char(byte @ 0x20..=0x7e) => out(&[*byte]),
        Datum::Char(byte) => out(&[
            b'\\',
            b'0' + (byte >> 6),
            b'0' + ((byte >> 3) & 7),
            b'0' + (byte & 7),
        ]),
        Datum::Xml(text) => crate::sql::xml::write_output(text, |part| out(part.as_bytes())),
        Datum::Bytea(bytes) => {
            use core::fmt::Write as _;
            let _ = write!(
                Writer(out),
                "{}",
                crate::sql::types::PgByteaRendered {
                    bytes,
                    escape: render.bytea_escape,
                }
            );
        }
        Datum::Date(date) => out(
            crate::sql::datetime::format_date_styled(*date, render.datestyle)
                .as_str()
                .as_bytes(),
        ),
        Datum::Timestamp(timestamp) | Datum::Timestamptz(timestamp) => {
            out(crate::sql::datetime::format_timestamp_styled(
                *timestamp,
                matches!(value, Datum::Timestamptz(_)),
                render.datestyle,
                render.parsed_timezone,
            )
            .as_str()
            .as_bytes())
        }
        Datum::Interval(interval) => out(crate::sql::datetime::format_interval_styled(
            *interval,
            render.intervalstyle,
        )
        .as_str()
        .as_bytes()),
        Datum::Float4(value) => {
            use core::fmt::Write as _;
            let _ = write!(
                Writer(out),
                "{}",
                crate::sql::types::PgFloat4Rendered(*value, render.extra_float_digits)
            );
        }
        Datum::Float8(value) => {
            use core::fmt::Write as _;
            let _ = write!(
                Writer(out),
                "{}",
                crate::sql::types::PgFloat8Rendered(*value, render.extra_float_digits)
            );
        }
        Datum::Array { element, raw } => write_wire_array(*element, raw, render, out),
        Datum::Record(fields) | Datum::Composite { fields, .. } => {
            out(b"(");
            for (index, field) in fields.iter().enumerate() {
                if index > 0 {
                    out(b",");
                }
                write_wire_record_field(&field.value, render, out);
            }
            out(b")");
        }
        other => {
            use core::fmt::Write as _;
            let _ = write!(Writer(out), "{other}");
        }
    }
}

fn write_wire_record_field(
    value: &Datum,
    render: crate::sql::guc::RenderContext,
    out: &mut dyn FnMut(&[u8]),
) {
    if value.is_null() {
        return;
    }
    let mut empty = true;
    let mut quote = false;
    write_wire_text(value, render, &mut |bytes| {
        if !bytes.is_empty() {
            empty = false;
        }
        quote |= bytes.iter().any(|byte| {
            matches!(byte, b',' | b'(' | b')' | b'"' | b'\\') || byte.is_ascii_whitespace()
        });
    });
    if quote || empty {
        out(b"\"");
    }
    write_wire_text(value, render, &mut |bytes| {
        let mut plain = 0;
        for (index, byte) in bytes.iter().enumerate() {
            if matches!(byte, b'"' | b'\\') {
                out(&bytes[plain..index]);
                // `record_out` doubles these bytes inside its quoted fields;
                // array output uses a different, backslash-based grammar.
                out(&[*byte, *byte]);
                plain = index + 1;
            }
        }
        out(&bytes[plain..]);
    });
    if quote || empty {
        out(b"\"");
    }
}

fn write_wire_array_element(
    value: Datum,
    delimiter: u8,
    render: crate::sql::guc::RenderContext,
    out: &mut dyn FnMut(&[u8]),
) {
    if value.is_null() {
        out(b"NULL");
        return;
    }
    let mut empty = true;
    let mut quote = false;
    let mut prefix = [0u8; 4];
    let mut prefix_len = 0;
    write_wire_text(&value, render, &mut |bytes| {
        if !bytes.is_empty() {
            empty = false;
        }
        quote |= bytes.iter().any(|byte| {
            *byte == delimiter
                || matches!(byte, b'{' | b'}' | b'"' | b'\\')
                || byte.is_ascii_whitespace()
        });
        for byte in bytes {
            if prefix_len < prefix.len() {
                prefix[prefix_len] = *byte;
            }
            prefix_len += 1;
        }
    });
    quote |= prefix_len == 4 && prefix.eq_ignore_ascii_case(b"null");
    if quote || empty {
        out(b"\"");
    }
    write_wire_text(&value, render, &mut |bytes| {
        let mut plain = 0;
        for (index, byte) in bytes.iter().enumerate() {
            if matches!(byte, b'"' | b'\\') {
                out(&bytes[plain..index]);
                out(b"\\");
                plain = index;
            }
        }
        out(&bytes[plain..]);
    });
    if quote || empty {
        out(b"\"");
    }
}

fn write_wire_array(
    element: crate::sql::types::ArrElem,
    raw: &[u8],
    render: crate::sql::guc::RenderContext,
    out: &mut dyn FnMut(&[u8]),
) {
    let shape = crate::sql::array::shape(raw).expect("array datum invariant");
    if shape.dimension_count() == 0 {
        out(b"{}");
        return;
    }
    if (0..shape.dimension_count()).any(|index| shape.lower_bound(index) != Some(1)) {
        for index in 0..shape.dimension_count() {
            let bound = stack_format!(
                32,
                "[{}:{}]",
                shape.lower_bound(index).unwrap(),
                shape.upper_bound(index).unwrap()
            );
            out(bound.as_str().as_bytes());
        }
        out(b"=");
    }
    fn level(
        element: crate::sql::types::ArrElem,
        shape: crate::sql::array::Shape,
        depth: usize,
        elements: &mut crate::sql::array::Elements<'_>,
        render: crate::sql::guc::RenderContext,
        out: &mut dyn FnMut(&[u8]),
    ) {
        out(b"{");
        for member in 0..shape.dimension(depth).unwrap() {
            if member > 0 {
                out(&[element.delimiter()]);
            }
            if depth + 1 == shape.dimension_count() {
                write_wire_array_element(
                    elements.next().expect("array datum carries every member"),
                    element.delimiter(),
                    render,
                    out,
                );
            } else {
                level(element, shape, depth + 1, elements, render, out);
            }
        }
        out(b"}");
    }
    let mut elements = crate::sql::array::elements(raw, element);
    level(element, shape, 0, &mut elements, render, out);
}

fn wire_text_len(value: &Datum, render: crate::sql::guc::RenderContext) -> usize {
    let mut len = 0;
    write_wire_text(value, render, &mut |bytes| len += bytes.len());
    len
}

fn text_value_len(value: &Datum, render: crate::sql::guc::RenderContext) -> usize {
    match value {
        Datum::Null => 0,
        Datum::PgDdlCommand => unreachable!("pg_ddl_command output is rejected before encoding"),
        Datum::Char(0) => 0,
        Datum::Char(0x20..=0x7e) => 1,
        Datum::Char(_) => 4,
        Datum::Array { .. } | Datum::Record(_) | Datum::Composite { .. } => {
            wire_text_len(value, render)
        }
        Datum::Xml(_) => wire_text_len(value, render),
        Datum::Text(text)
        | Datum::Bpchar(text)
        | Datum::Regtype { name: text, .. }
        | Datum::RegObject { name: text, .. } => text.len(),
        Datum::Bytea(bytes) => crate::sql::types::PgByteaRendered {
            bytes,
            escape: render.bytea_escape,
        }
        .text_len(),
        Datum::Numeric(number) => display_len(number),
        Datum::Float4(value) => display_len(crate::sql::types::PgFloat4Rendered(
            *value,
            render.extra_float_digits,
        )),
        Datum::Float8(value) => display_len(crate::sql::types::PgFloat8Rendered(
            *value,
            render.extra_float_digits,
        )),
        Datum::Date(date) => crate::sql::datetime::format_date_styled(*date, render.datestyle)
            .as_str()
            .len(),
        Datum::Timestamp(timestamp) | Datum::Timestamptz(timestamp) => {
            crate::sql::datetime::format_timestamp_styled(
                *timestamp,
                matches!(value, Datum::Timestamptz(_)),
                render.datestyle,
                render.parsed_timezone,
            )
            .as_str()
            .len()
        }
        Datum::Interval(interval) => {
            crate::sql::datetime::format_interval_styled(*interval, render.intervalstyle)
                .as_str()
                .len()
        }
        other => display_len(other),
    }
}

fn binary_value_len(value: &Datum) -> usize {
    match value {
        Datum::Null => 0,
        Datum::PgDdlCommand => unreachable!("pg_ddl_command output is rejected before encoding"),
        Datum::Bool(_) => 1,
        Datum::Char(_) => 1,
        Datum::Int2(_) => 2,
        Datum::Int4(_)
        | Datum::Oid(_)
        | Datum::Cid(_)
        | Datum::Regtype { .. }
        | Datum::RegObject { .. }
        | Datum::Date(_)
        | Datum::Float4(_) => 4,
        Datum::Tid(_) | Datum::Macaddr(_) => 6,
        Datum::Int8(_)
        | Datum::Xid8(_)
        | Datum::PgLsn(_)
        | Datum::Money(_)
        | Datum::Timestamp(_)
        | Datum::Timestamptz(_)
        | Datum::Time(_)
        | Datum::Float8(_)
        | Datum::Macaddr8(_) => 8,
        Datum::Timetz(..) => 12,
        Datum::Interval(_) | Datum::Uuid(_) => 16,
        Datum::Text(text) | Datum::Bpchar(text) => text.len(),
        Datum::PgNodeTree(node) => node.as_str().len(),
        Datum::AclItem(_) => unreachable!("aclitem has no binary output function"),
        Datum::Xml(text) => {
            let mut length = 0usize;
            crate::sql::xml::write_output(text, |part| length = length.saturating_add(part.len()));
            length
        }
        Datum::TsVector(text) => crate::sql::full_text::emit_vector_binary(text.as_str(), |_| {}),
        Datum::TsQuery(text) => crate::sql::full_text::emit_query_binary(text.as_str(), |_| {}),
        Datum::Bytea(bytes) => bytes.len(),
        Datum::Json { text, jsonb } => text.len().saturating_add(usize::from(*jsonb)),
        Datum::JsonPath(text) => 1usize.saturating_add(text.len()),
        Datum::Snapshot { value, .. } => value.raw().len(),
        Datum::Range { text, .. } | Datum::Multirange { text, .. } => text.len(),
        Datum::Geometry { kind, text } => {
            crate::sql::geometry::binary_len(*kind, text).expect("geometry datum is canonical")
        }
        Datum::Bit { bits, .. } => 4usize.saturating_add(bits.len().div_ceil(8)),
        Datum::Inet(network) | Datum::Cidr(network) => 4usize.saturating_add(network.addr_len()),
        Datum::Enum { label, .. } => label.len(),
        Datum::Numeric(number) => 8usize.saturating_add(number.ndigits().saturating_mul(2)),
        Datum::Record(_) | Datum::Composite { .. } | Datum::CompositeText { .. } => {
            display_len(value)
        }
        Datum::Int2Vector { raw, .. } => {
            let count = raw.len() / 2;
            20usize.saturating_add(count.saturating_mul(6))
        }
        Datum::OidVector { raw, .. } => {
            let count = raw.len() / 4;
            20usize.saturating_add(count.saturating_mul(8))
        }
        Datum::Array { element, raw } => {
            let shape = crate::sql::array::shape(raw).expect("array datum invariant");
            let mut bytes = 12usize.saturating_add(shape.dimension_count().saturating_mul(8));
            for item in crate::sql::array::elements(raw, *element) {
                bytes = bytes
                    .saturating_add(4)
                    .saturating_add(binary_value_len(&item));
            }
            bytes
        }
    }
}

fn serialized_row_len(
    values: &[Datum],
    binary: bool,
    render: crate::sql::guc::RenderContext,
) -> usize {
    values.iter().fold(2usize, |bytes, value| {
        bytes.saturating_add(4).saturating_add(if binary {
            binary_value_len(value)
        } else {
            text_value_len(value, render)
        })
    })
}

impl<'b> Responder<'b> {
    pub fn new(buffer: &'b mut FixedBuf) -> Self {
        Self {
            buffer,
            cursor_rows: CursorRowOutput::Wire,
            suppress_row_description: false,
            formats: ResultFmt::ALL_TEXT,
            result_type_oids: [0; MAX_RESULT_COLS],
            result_columns: 0,
            output: None,
            render: crate::sql::guc::RenderContext::default(),
            discard_query_output: false,
            suppress_command_complete: false,
            last_command_completion: None,
            affected_rows: None,
            discarded_rows: 0,
            discard_serialize: ExplainSerialize::None,
            serialized_bytes: 0,
            serialization_micros: 0,
        }
    }

    pub fn for_execute(buffer: &'b mut FixedBuf, formats: ResultFmt) -> Self {
        Self {
            buffer,
            cursor_rows: CursorRowOutput::Wire,
            suppress_row_description: true,
            formats,
            result_type_oids: [0; MAX_RESULT_COLS],
            result_columns: 0,
            output: None,
            render: crate::sql::guc::RenderContext::default(),
            discard_query_output: false,
            suppress_command_complete: false,
            last_command_completion: None,
            affected_rows: None,
            discarded_rows: 0,
            discard_serialize: ExplainSerialize::None,
            serialized_bytes: 0,
            serialization_micros: 0,
        }
    }

    /// Saves cursor values or discards them without rendering.
    pub(crate) fn for_cursor(description: &'b mut FixedBuf, rows: CursorRowOutput<'b>) -> Self {
        Self {
            buffer: description,
            cursor_rows: rows,
            suppress_row_description: false,
            formats: ResultFmt::ALL_TEXT,
            result_type_oids: [0; MAX_RESULT_COLS],
            result_columns: 0,
            output: None,
            render: crate::sql::guc::RenderContext::default(),
            discard_query_output: false,
            suppress_command_complete: true,
            last_command_completion: None,
            affected_rows: None,
            discarded_rows: 0,
            discard_serialize: ExplainSerialize::None,
            serialized_bytes: 0,
            serialization_micros: 0,
        }
    }

    /// Describe on a portal: RowDescription is emitted with the portal's
    /// result-format codes so the client decodes DataRows correctly.
    pub fn for_describe(buffer: &'b mut FixedBuf, formats: ResultFmt) -> Self {
        Self {
            buffer,
            cursor_rows: CursorRowOutput::Wire,
            suppress_row_description: false,
            formats,
            result_type_oids: [0; MAX_RESULT_COLS],
            result_columns: 0,
            output: None,
            render: crate::sql::guc::RenderContext::default(),
            discard_query_output: false,
            suppress_command_complete: false,
            last_command_completion: None,
            affected_rows: None,
            discarded_rows: 0,
            discard_serialize: ExplainSerialize::None,
            serialized_bytes: 0,
            serialization_micros: 0,
        }
    }

    /// Queues output that does not fit `buffer` in the connection's overflow
    /// area instead of failing the statement.
    pub(crate) fn with_output(mut self, sink: OutputSink<'b>) -> Self {
        self.output = Some(sink);
        self
    }

    /// Whether the client transport failed during this statement.
    pub(crate) fn transport_failed(&self) -> bool {
        self.output.as_ref().is_some_and(|sink| sink.failed)
    }

    /// Position in the logical output stream (send buffer, then overflow).
    pub(crate) fn mark(&self) -> OutputMark {
        match &self.output {
            Some(sink) => OutputMark::new(
                self.buffer.len() + sink.overflow.len(),
                sink.overflow.written(),
            ),
            None => OutputMark::new(self.buffer.len(), 0),
        }
    }

    fn excess_since(&self, mark: OutputMark) -> Option<usize> {
        match &self.output {
            Some(sink) => mark.excess(
                self.buffer.len() + sink.overflow.len(),
                sink.overflow.written(),
            ),
            None => mark.excess(self.buffer.len(), 0),
        }
    }

    /// Whether output after `mark` is still entirely unsent.
    pub(crate) fn can_truncate_to(&self, mark: OutputMark) -> bool {
        self.excess_since(mark).is_some()
    }

    /// Discards unsent output after `mark`. Output that already reached the
    /// client cannot be retracted; callers check [`Self::can_truncate_to`]
    /// where a prefix may have been written.
    pub(crate) fn truncate_to(&mut self, mark: OutputMark) {
        let excess = self
            .excess_since(mark)
            .expect("output after the mark is still unsent");
        let overflow = self.output.as_mut().map(|sink| &mut *sink.overflow);
        assert!(super::overflow::truncate_output(
            self.buffer,
            overflow,
            excess
        ));
    }

    /// The last `bytes` of queued output (one complete message or less).
    fn tail(&self, bytes: usize) -> &[u8] {
        if let Some(sink) = &self.output
            && !sink.overflow.is_empty()
        {
            return sink.overflow.tail(bytes).expect("queued tail");
        }
        let readable = self.buffer.readable();
        &readable[readable.len() - bytes..]
    }

    /// Sets the session value-rendering context (DateStyle / time zone).
    /// Updates the value-rendering context in place (e.g. after a SET changed
    /// DateStyle mid-batch).
    pub fn render_context(&self) -> crate::sql::guc::RenderContext {
        crate::sql::guc::active_render().unwrap_or(self.render)
    }

    pub fn set_render(&mut self, render: crate::sql::guc::RenderContext) {
        self.render = render;
    }

    pub(crate) fn begin_discard_query_output(&mut self, serialize: ExplainSerialize) {
        debug_assert!(!self.discard_query_output);
        self.discard_query_output = true;
        self.discarded_rows = 0;
        self.discard_serialize = serialize;
        self.serialized_bytes = 0;
        self.serialization_micros = 0;
    }

    pub(crate) fn finish_discard_query_output(&mut self) -> DiscardedOutput {
        debug_assert!(self.discard_query_output);
        self.discard_query_output = false;
        DiscardedOutput {
            rows: self.discarded_rows,
            serialized_bytes: self.serialized_bytes,
            serialization_micros: self.serialization_micros,
        }
    }

    /// Appends one complete message after all queued output. `build` may
    /// run more than once, so it must be deterministic.
    fn emit(
        &mut self,
        class: OutputClass,
        build: impl Fn(&mut FixedBuf) -> Result<(), WireFull>,
    ) -> Result<(), WireFull> {
        let Some(sink) = self.output.as_mut() else {
            return Self::append_message(self.buffer, &build);
        };
        if sink.promotion == Promotion::Requested {
            // The attempt is being discarded; nothing more of it may queue.
            return Err(WireFull);
        }
        if sink.overflow.is_empty() && Self::append_message(self.buffer, &build).is_ok() {
            return Ok(());
        }
        if class == OutputClass::Bulk
            && let Promotion::Armed { mark, epoch } = sink.promotion
        {
            // Before any byte of this statement leaves and before any effect,
            // the statement can stop and reopen as a resumable stream.
            let restartable = mark
                .excess(
                    self.buffer.len() + sink.overflow.len(),
                    sink.overflow.written(),
                )
                .is_some()
                && epoch == crate::sql::query::statement_effect_epoch()
                && crate::sql::sequence::ensure_statement_effects_replayable().is_ok();
            if restartable {
                sink.promotion = Promotion::Requested;
                return Err(WireFull);
            }
            sink.promotion = Promotion::Declined(SpoolReason::Effects);
        }
        // Retry a full send buffer at once; with output already queued, retry
        // once half a send buffer more has accumulated.
        if sink.overflow.is_empty() || sink.overflow.len() >= sink.next_write_at {
            sink.write_available(self.buffer);
            if sink.overflow.is_empty() && Self::append_message(self.buffer, &build).is_ok() {
                return Ok(());
            }
        }
        if class == OutputClass::Bulk
            && sink.spooled.is_none()
            && let Promotion::Declined(reason) = sink.promotion
        {
            sink.spooled = Some(reason);
        }
        let class = if sink.rows_use_reserve {
            OutputClass::Control
        } else {
            class
        };
        sink.overflow.push(sink.spool.as_ref(), class, &build)
    }

    /// Whether this output may resume a `SELECT` as a result stream.
    pub(crate) fn promotion_allowed(&self) -> bool {
        self.output
            .as_ref()
            .is_some_and(|sink| sink.promotion == Promotion::Allowed)
    }

    /// Starts a statement whose first overflow may request a result stream.
    pub(crate) fn arm_promotion(&mut self) {
        let mark = self.mark();
        if let Some(sink) = self.output.as_mut() {
            sink.promotion = Promotion::Armed {
                mark,
                epoch: crate::sql::query::statement_effect_epoch(),
            };
        }
    }

    /// Starts a statement whose overflow queues for `reason`.
    pub(crate) fn decline_promotion(&mut self, reason: SpoolReason) {
        if let Some(sink) = self.output.as_mut() {
            sink.promotion = Promotion::Declined(reason);
        }
    }

    /// Ends a statement's promotion decision: whether it requested a stream,
    /// and why its overflow queued instead.
    pub(crate) fn finish_promotion(&mut self) -> (bool, Option<SpoolReason>) {
        let Some(sink) = self.output.as_mut() else {
            return (false, None);
        };
        if sink.promotion == Promotion::Disabled {
            return (false, None);
        }
        let requested = sink.promotion == Promotion::Requested;
        sink.promotion = Promotion::Allowed;
        (requested, sink.spooled.take())
    }

    /// Writes what the client accepts, then reports whether a result stream
    /// should wait for its client before evaluating another row.
    pub(crate) fn output_backlogged(&mut self) -> bool {
        let Some(sink) = self.output.as_mut() else {
            return false;
        };
        sink.write_available(self.buffer);
        !sink.overflow.is_empty() || self.buffer.writable().len() < self.buffer.capacity() / 2
    }

    fn append_message(
        buffer: &mut FixedBuf,
        build: &dyn Fn(&mut FixedBuf) -> Result<(), WireFull>,
    ) -> Result<(), WireFull> {
        // Compaction can move unread bytes; save the boundary afterwards.
        let _ = buffer.writable();
        let mark = buffer.mark();
        build(buffer).inspect_err(|_| buffer.truncate_to(mark))
    }

    fn with_retry(
        &mut self,
        build: impl Fn(&mut FixedBuf) -> Result<(), WireFull>,
    ) -> Result<(), WireFull> {
        self.emit(OutputClass::Bulk, build)
    }

    fn control(
        &mut self,
        build: impl Fn(&mut FixedBuf) -> Result<(), WireFull>,
    ) -> Result<(), WireFull> {
        self.emit(OutputClass::Control, build)
    }

    pub fn parse_complete(&mut self) -> Result<(), WireFull> {
        self.control(|buffer| MsgOut::begin(buffer, wire::MSG_PARSE_COMPLETE).finish())
    }

    pub fn bind_complete(&mut self) -> Result<(), WireFull> {
        self.control(|buffer| MsgOut::begin(buffer, wire::MSG_BIND_COMPLETE).finish())
    }

    pub fn close_complete(&mut self) -> Result<(), WireFull> {
        self.control(|buffer| MsgOut::begin(buffer, wire::MSG_CLOSE_COMPLETE).finish())
    }

    pub fn portal_suspended(&mut self) -> Result<(), WireFull> {
        self.control(|buffer| MsgOut::begin(buffer, wire::MSG_PORTAL_SUSPENDED).finish())
    }

    pub fn no_data(&mut self) -> Result<(), WireFull> {
        self.control(|buffer| MsgOut::begin(buffer, wire::MSG_NO_DATA).finish())
    }

    pub(crate) fn function_call_response(
        &mut self,
        value: &Datum,
        binary: bool,
    ) -> Result<(), WireFull> {
        let render = self.render_context();
        self.with_retry(|buffer| {
            let mut message = MsgOut::begin(buffer, wire::MSG_FUNCTION_CALL_RESPONSE);
            if binary {
                Self::encode_value_binary(&mut message, value);
            } else {
                Self::encode_value_text(&mut message, value, render);
            }
            message.finish()
        })
    }

    /// Reports every inferred or client-specified parameter type OID.
    pub fn parameter_description(&mut self, oids: &[i32]) -> Result<(), WireFull> {
        self.control(|buffer| {
            let mut m = MsgOut::begin(buffer, wire::MSG_PARAMETER_DESCRIPTION);
            m.u16(oids.len() as u16);
            for &oid in oids {
                m.i32(oid);
            }
            m.finish()
        })
    }

    pub fn auth_ok(&mut self) -> Result<(), WireFull> {
        self.control(|buffer| {
            let mut m = MsgOut::begin(buffer, wire::MSG_AUTHENTICATION);
            m.i32(wire::AUTH_OK);
            m.finish()
        })
    }

    pub fn auth_cleartext_password(&mut self) -> Result<(), WireFull> {
        self.control(|buffer| {
            let mut m = MsgOut::begin(buffer, wire::MSG_AUTHENTICATION);
            m.i32(wire::AUTH_CLEARTEXT);
            m.finish()
        })
    }

    pub fn auth_md5_password(&mut self, salt: &[u8; 4]) -> Result<(), WireFull> {
        self.control(|buffer| {
            let mut m = MsgOut::begin(buffer, wire::MSG_AUTHENTICATION);
            m.i32(wire::AUTH_MD5);
            m.bytes(salt);
            m.finish()
        })
    }

    pub fn auth_sasl_mechanisms(&mut self) -> Result<(), WireFull> {
        self.control(|buffer| {
            let mut m = MsgOut::begin(buffer, wire::MSG_AUTHENTICATION);
            m.i32(wire::AUTH_SASL);
            m.cstr("SCRAM-SHA-256");
            m.u8(0); // end of mechanism list
            m.finish()
        })
    }

    pub fn auth_sasl_continue(&mut self, payload: &str) -> Result<(), WireFull> {
        self.control(|buffer| {
            let mut m = MsgOut::begin(buffer, wire::MSG_AUTHENTICATION);
            m.i32(wire::AUTH_SASL_CONTINUE);
            m.bytes(payload.as_bytes());
            m.finish()
        })
    }

    pub fn auth_sasl_final(&mut self, payload: &str) -> Result<(), WireFull> {
        self.control(|buffer| {
            let mut m = MsgOut::begin(buffer, wire::MSG_AUTHENTICATION);
            m.i32(wire::AUTH_SASL_FINAL);
            m.bytes(payload.as_bytes());
            m.finish()
        })
    }

    pub fn parameter_status(&mut self, name: &str, value: &str) -> Result<(), WireFull> {
        self.control(|buffer| {
            let mut m = MsgOut::begin(buffer, wire::MSG_PARAMETER_STATUS);
            m.cstr(name).cstr(value);
            m.finish()
        })
    }

    pub fn backend_key_data(&mut self, pid: i32, key: &[u8]) -> Result<(), WireFull> {
        self.control(|buffer| {
            let mut m = MsgOut::begin(buffer, wire::MSG_BACKEND_KEY_DATA);
            m.i32(pid).bytes(key);
            m.finish()
        })
    }

    pub fn negotiate_protocol_version(
        &mut self,
        newest_minor: i32,
        unrecognized_options: &[&str],
    ) -> Result<(), WireFull> {
        self.control(|buffer| {
            let mut m = MsgOut::begin(buffer, wire::MSG_NEGOTIATE_VERSION);
            m.i32(newest_minor);
            m.i32(unrecognized_options.len() as i32);
            for opt in unrecognized_options {
                m.cstr(opt);
            }
            m.finish()
        })
    }

    pub fn ready_for_query(&mut self, tx_status: u8) -> Result<(), WireFull> {
        self.control(|buffer| {
            let mut m = MsgOut::begin(buffer, wire::MSG_READY_FOR_QUERY);
            m.u8(tx_status);
            m.finish()
        })
    }

    pub fn row_description(&mut self, columns: &[ColDesc]) -> Result<(), WireFull> {
        self.result_columns = columns.len().min(MAX_RESULT_COLS);
        for (target, column) in self
            .result_type_oids
            .iter_mut()
            .zip(columns.iter())
            .take(self.result_columns)
        {
            *target = column.type_oid;
        }
        if self.suppress_row_description || self.discard_query_output {
            return Ok(());
        }
        let formats = self.formats;
        self.with_retry(|buffer| {
            let mut m = MsgOut::begin(buffer, wire::MSG_ROW_DESCRIPTION);
            m.i16(columns.len() as i16);
            for (i, c) in columns.iter().enumerate() {
                m.cstr(c.name);
                m.i32(0);
                m.i16(0);
                m.i32(c.type_oid);
                m.i16(c.typlen);
                m.i32(c.type_mod);
                m.i16(if formats.is_binary(i) { 1 } else { 0 });
            }
            m.finish()
        })?;
        Ok(())
    }

    /// CopyInResponse / CopyOutResponse. `binary` selects PostgreSQL's format
    /// code (1 = binary, 0 = text/CSV) for the overall message and every column.
    pub fn copy_in_response(&mut self, n_columns: usize, binary: bool) -> Result<(), WireFull> {
        self.copy_response(wire::MSG_COPY_IN_RESPONSE, n_columns, binary)
    }

    pub fn copy_out_response(&mut self, n_columns: usize, binary: bool) -> Result<(), WireFull> {
        self.copy_response(wire::MSG_COPY_OUT_RESPONSE, n_columns, binary)
    }

    /// CopyBothResponse enters PostgreSQL's replication COPY mode. Logical
    /// replication has no relation columns; pgoutput's binary envelopes are
    /// carried inside later CopyData frames.
    pub fn copy_both_response(&mut self) -> Result<(), WireFull> {
        self.copy_response(wire::MSG_COPY_BOTH_RESPONSE, 0, false)
    }

    fn copy_response(&mut self, kind: u8, n_columns: usize, binary: bool) -> Result<(), WireFull> {
        let format = u8::from(binary);
        self.with_retry(|buffer| {
            let mut m = MsgOut::begin(buffer, kind);
            m.u8(format);
            m.i16(n_columns as i16);
            for _ in 0..n_columns {
                m.i16(i16::from(format));
            }
            m.finish()
        })
    }

    /// One text/CSV CopyData row: `write` emits the fields and separators; the
    /// trailing newline is appended here. `write` may run twice (the
    /// flush-and-retry path), so it must be deterministic.
    pub fn copy_data_row(&mut self, write: &CopyRowWriter<'_>) -> Result<(), WireFull> {
        self.with_retry(|buffer| {
            let mut m = MsgOut::begin(buffer, b'd');
            write(&mut |bytes| {
                m.bytes(bytes);
            });
            m.bytes(b"\n");
            m.finish()
        })
    }

    /// One raw binary replication CopyData frame. Unlike COPY rows, logical
    /// replication payloads carry their own message boundaries and no newline.
    pub fn copy_data(&mut self, write: &dyn Fn(&mut MsgOut)) -> Result<(), WireFull> {
        self.with_retry(|buffer| {
            let mut message = MsgOut::begin(buffer, b'd');
            write(&mut message);
            message.finish()
        })
    }

    /// The binary COPY file header: the 11-byte signature, an int32 flags word,
    /// and an int32 header-extension length (both zero). One CopyData message.
    pub fn copy_binary_header(&mut self) -> Result<(), WireFull> {
        self.with_retry(|buffer| {
            let mut m = MsgOut::begin(buffer, b'd');
            m.bytes(b"PGCOPY\n\xff\r\n\0");
            m.i32(0);
            m.i32(0);
            m.finish()
        })
    }

    /// One binary CopyData row: the int16 field count, then `write` emits each
    /// field as its int32 length (or -1 for NULL) followed by the value's binary
    /// bytes (via [`Self::encode_value_binary`]). `write` may run twice on the
    /// flush-and-retry path, so it must be deterministic.
    pub fn copy_binary_row(
        &mut self,
        n_fields: usize,
        write: &dyn Fn(&mut MsgOut),
    ) -> Result<(), WireFull> {
        self.with_retry(|buffer| {
            let mut m = MsgOut::begin(buffer, b'd');
            m.i16(n_fields as i16);
            write(&mut m);
            m.finish()
        })
    }

    /// The binary COPY trailer: an int16 field count of -1. One CopyData message.
    pub fn copy_binary_trailer(&mut self) -> Result<(), WireFull> {
        self.with_retry(|buffer| {
            let mut m = MsgOut::begin(buffer, b'd');
            m.i16(-1);
            m.finish()
        })
    }

    pub fn copy_done(&mut self) -> Result<(), WireFull> {
        self.with_retry(|buffer| MsgOut::begin(buffer, b'c').finish())
    }

    pub fn data_row(&mut self, values: &[Datum]) -> Result<(), WireFull> {
        if self.discard_query_output {
            self.discarded_rows = self.discarded_rows.saturating_add(1);
            if self.discard_serialize != ExplainSerialize::None {
                let started = std::time::Instant::now();
                let binary = self.discard_serialize == ExplainSerialize::Binary;
                let bytes = serialized_row_len(values, binary, self.render_context());
                self.serialized_bytes = self.serialized_bytes.saturating_add(bytes as u64);
                self.serialization_micros = self
                    .serialization_micros
                    .saturating_add(started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64);
            }
            return Ok(());
        }
        let formats = self.formats;
        let render = self.render_context();
        self.with_retry(|buffer| Self::build_data_row(buffer, values, formats, render))
    }

    /// Emits a row whose fields were prepared by the executor. The writer must
    /// emit each field's length prefix and bytes, and remain deterministic when
    /// the transport drains and retries the message.
    pub(crate) fn data_row_prepared(
        &mut self,
        values: &[Datum],
        write: &dyn Fn(&mut MsgOut),
    ) -> Result<(), WireFull> {
        if self.discard_query_output {
            self.discarded_rows = self.discarded_rows.saturating_add(1);
            if self.discard_serialize != ExplainSerialize::None {
                let started = std::time::Instant::now();
                let binary = self.discard_serialize == ExplainSerialize::Binary;
                let bytes = serialized_row_len(values, binary, self.render_context());
                self.serialized_bytes = self.serialized_bytes.saturating_add(bytes as u64);
                self.serialization_micros = self
                    .serialization_micros
                    .saturating_add(started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64);
            }
            return Ok(());
        }
        self.with_retry(|buffer| {
            let mut m = MsgOut::begin(buffer, wire::MSG_DATA_ROW);
            m.i16(values.len() as i16);
            write(&mut m);
            m.finish()
        })
    }

    pub(crate) fn cursor_row_output(&mut self) -> CursorRowOutput<'_> {
        match &mut self.cursor_rows {
            CursorRowOutput::Wire => CursorRowOutput::Wire,
            CursorRowOutput::Save(rows) => CursorRowOutput::Save(rows),
            CursorRowOutput::Discard => CursorRowOutput::Discard,
        }
    }

    pub(crate) fn result_formats(&self) -> ResultFmt {
        self.formats
    }

    /// A binary SQL cursor must not change the next statement's wire format.
    pub(crate) fn result_format_scope(&mut self, formats: ResultFmt) -> ResultFormatScope<'_, 'b> {
        let previous = core::mem::replace(&mut self.formats, formats);
        ResultFormatScope {
            responder: self,
            previous,
        }
    }

    /// Reinstalls a resumed result's column types without resending its
    /// RowDescription.
    pub(crate) fn restore_result_types(&mut self, type_oids: &[i32]) {
        self.result_columns = type_oids.len().min(MAX_RESULT_COLS);
        self.result_type_oids[..self.result_columns]
            .copy_from_slice(&type_oids[..self.result_columns]);
    }

    pub(crate) fn result_type_oid(&self, index: usize) -> Option<i32> {
        (index < self.result_columns).then(|| self.result_type_oids[index])
    }

    /// Rewrites a captured SQL-cursor RowDescription for this FETCH's result
    /// formats. Cursor sealing has already validated the message boundary.
    pub(crate) fn cursor_row_description(
        &mut self,
        captured: &[u8],
        formats: ResultFmt,
    ) -> Result<(), WireFull> {
        let payload = &captured[5..];
        let count = i16::from_be_bytes(payload[..2].try_into().unwrap()) as usize;
        self.result_columns = count;
        let mut at = 2usize;
        for column in 0..count {
            let name_end = payload[at..].iter().position(|byte| *byte == 0).unwrap() + at + 1;
            self.result_type_oids[column] =
                i32::from_be_bytes(payload[name_end + 6..name_end + 10].try_into().unwrap());
            at = name_end + 18;
        }
        if self.suppress_row_description || self.discard_query_output {
            return Ok(());
        }
        self.with_retry(|buffer| {
            let mut message = MsgOut::begin(buffer, wire::MSG_ROW_DESCRIPTION);
            message.i16(count as i16);
            let mut at = 2usize;
            for column in 0..count {
                let name_end = payload[at..].iter().position(|byte| *byte == 0).unwrap() + at + 1;
                message.bytes(&payload[at..name_end + 16]);
                message.i16(if formats.is_binary(column) { 1 } else { 0 });
                at = name_end + 18;
            }
            message.finish()
        })
    }

    /// Emits one row, each column in its Bind-requested text or binary format.
    fn build_data_row(
        buffer: &mut FixedBuf,
        values: &[Datum],
        formats: ResultFmt,
        render: crate::sql::guc::RenderContext,
    ) -> Result<(), WireFull> {
        let mut m = MsgOut::begin(buffer, wire::MSG_DATA_ROW);
        m.i16(values.len() as i16);
        for (i, v) in values.iter().enumerate() {
            if v.is_null() {
                m.i32(-1);
            } else if formats.is_binary(i) {
                Self::encode_value_binary(&mut m, v);
            } else {
                Self::encode_value_text(&mut m, v, render);
            }
        }
        m.finish()
    }

    pub(crate) fn encode_value_text(
        message: &mut MsgOut,
        value: &Datum,
        render: crate::sql::guc::RenderContext,
    ) {
        if value.is_null() {
            message.i32(-1);
        } else {
            message.field(|message| {
                write_wire_text(value, render, &mut |bytes| {
                    message.bytes(bytes);
                })
            });
        }
    }

    /// One value's wire-text form as arena bytes — the same output
    /// function semantics `encode_value_text` streams onto the wire (styled
    /// dates and timestamps, GUC-honoring bytea, Display for the rest), for
    /// consumers that must post-process the text (COPY escapes it).
    /// `None` is NULL.
    pub fn datum_wire_text<'a>(
        v: &Datum<'a>,
        render: crate::sql::guc::RenderContext,
        arena: &'a crate::mem::arena::Arena,
    ) -> Result<Option<&'a [u8]>, crate::sql::eval::SqlError> {
        use crate::sql::eval::sqlstate;
        let full = || {
            crate::sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "COPY row exceeds the statement arena"
            )
        };
        Ok(Some(match v {
            Datum::Null => return Ok(None),
            Datum::PgNodeTree(node) => node.as_str().as_bytes(),
            Datum::PgDdlCommand => {
                return Err(crate::sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "cannot display a value of type pg_ddl_command"
                ));
            }
            Datum::Text(text)
            | Datum::Bpchar(text)
            | Datum::Regtype { name: text, .. }
            | Datum::RegObject { name: text, .. } => text.as_bytes(),
            value => {
                let length = wire_text_len(value, render);
                let bytes = arena
                    .alloc_slice_with(length, |_| 0u8)
                    .map_err(|_| full())?;
                let mut at = 0usize;
                write_wire_text(value, render, &mut |part| {
                    bytes[at..at + part.len()].copy_from_slice(part);
                    at += part.len();
                });
                debug_assert_eq!(at, length);
                &*bytes
            }
        }))
    }

    /// Binary wire representations, per PostgreSQL's send functions:
    /// network byte order, dates as days and timestamps as microseconds
    /// since 2000-01-01. Writes the int32 length prefix (or -1 for NULL) then
    /// the value bytes, so it doubles as a COPY-binary field writer.
    pub(crate) fn encode_value_binary(m: &mut MsgOut, v: &Datum) {
        {
            match v {
                Datum::Null => {
                    m.i32(-1);
                }
                Datum::PgDdlCommand => {
                    unreachable!("pg_ddl_command output is rejected before encoding")
                }
                Datum::AclItem(_) => {
                    unreachable!("aclitem has no binary output function")
                }
                Datum::PgNodeTree(node) => {
                    m.i32(node.as_str().len() as i32);
                    m.bytes(node.as_str().as_bytes());
                }
                Datum::Bool(b) => {
                    m.i32(1);
                    m.u8(u8::from(*b));
                }
                Datum::Int4(x) => {
                    m.i32(4);
                    m.bytes(&x.to_be_bytes());
                }
                Datum::Oid(x) => {
                    m.i32(4);
                    m.bytes(&x.to_be_bytes());
                }
                Datum::Tid(x) => {
                    m.i32(6);
                    m.bytes(&x.block.to_be_bytes());
                    m.bytes(&x.offset.to_be_bytes());
                }
                Datum::Cid(x) => {
                    m.i32(4);
                    m.bytes(&x.to_be_bytes());
                }
                Datum::Xid8(x) => {
                    m.i32(8);
                    m.bytes(&x.to_be_bytes());
                }
                Datum::Snapshot { value, .. } => {
                    m.i32(value.raw().len() as i32);
                    m.bytes(value.raw());
                }
                Datum::PgLsn(x) => {
                    m.i32(8);
                    m.bytes(&x.to_be_bytes());
                }
                Datum::Money(x) => {
                    m.i32(8);
                    m.bytes(&x.to_be_bytes());
                }
                Datum::Regtype { referenced_oid, .. } => {
                    m.i32(4);
                    m.bytes(&referenced_oid.to_be_bytes());
                }
                Datum::RegObject { referenced_oid, .. } => {
                    m.i32(4);
                    m.bytes(&referenced_oid.to_be_bytes());
                }
                Datum::Int2(x) => {
                    m.i32(2);
                    m.bytes(&x.to_be_bytes());
                }
                Datum::Date(x) => {
                    m.i32(4);
                    m.bytes(&x.to_be_bytes());
                }
                Datum::Int8(x) | Datum::Timestamp(x) | Datum::Timestamptz(x) | Datum::Time(x) => {
                    m.i32(8);
                    m.bytes(&x.to_be_bytes());
                }
                Datum::Timetz(t, zone) => {
                    // 8 bytes of time then the zone, which PostgreSQL counts
                    // west of UTC — the opposite sign to the stored offset.
                    m.i32(12);
                    m.bytes(&t.to_be_bytes());
                    m.bytes(&(-*zone).to_be_bytes());
                }
                Datum::Interval(interval) => {
                    // PostgreSQL binary interval: int64 micros, int32 days, int32 months.
                    m.i32(16);
                    m.bytes(&interval.micros.to_be_bytes());
                    m.bytes(&interval.days.to_be_bytes());
                    m.bytes(&interval.months.to_be_bytes());
                }
                Datum::Geometry { kind, text } => {
                    m.field(|m| {
                        crate::sql::geometry::emit_binary(*kind, text, |bytes| {
                            m.bytes(bytes);
                        })
                        .expect("geometry datum is canonical");
                    });
                }
                Datum::Json { text, jsonb } => {
                    // json binary is the text; jsonb prefixes a version byte (1).
                    if *jsonb {
                        m.i32(text.len() as i32 + 1);
                        m.bytes(&[1]);
                    } else {
                        m.i32(text.len() as i32);
                    }
                    m.bytes(text.as_bytes());
                }
                Datum::Xml(text) => {
                    m.field(|m| {
                        crate::sql::xml::write_output(text, |part| {
                            m.bytes(part.as_bytes());
                        });
                    });
                }
                Datum::JsonPath(text) => {
                    m.i32(text.len() as i32 + 1);
                    m.u8(1);
                    m.bytes(text.as_bytes());
                }
                Datum::Range { text, kind } => Self::encode_range_binary(m, text, *kind),
                Datum::Multirange { text, kind } => Self::encode_multirange_binary(m, text, *kind),
                Datum::Bit { bits, .. } => {
                    // int32 bit length, then ceil(len/8) bytes, bits packed
                    // MSB-first with the last byte's low bits zero-padded.
                    m.field(|m| {
                        m.i32(bits.len() as i32);
                        let mut byte = 0u8;
                        let mut fill = 0u32;
                        for ch in bits.bytes() {
                            byte = (byte << 1) | u8::from(ch == b'1');
                            fill += 1;
                            if fill == 8 {
                                m.u8(byte);
                                byte = 0;
                                fill = 0;
                            }
                        }
                        if fill > 0 {
                            m.u8(byte << (8 - fill));
                        }
                    });
                }
                Datum::Float4(x) => {
                    m.i32(4);
                    m.bytes(&x.to_bits().to_be_bytes());
                }
                Datum::Float8(x) => {
                    m.i32(8);
                    m.bytes(&x.to_bits().to_be_bytes());
                }
                Datum::Char(byte) => {
                    m.i32(1);
                    m.u8(*byte);
                }
                Datum::Text(s) | Datum::Bpchar(s) => {
                    m.i32(s.len() as i32);
                    m.bytes(s.as_bytes());
                }
                Datum::TsVector(text) => {
                    m.field(|m| {
                        crate::sql::full_text::emit_vector_binary(text.as_str(), |bytes| {
                            m.bytes(bytes);
                        });
                    });
                }
                Datum::TsQuery(text) => {
                    m.field(|m| {
                        crate::sql::full_text::emit_query_binary(text.as_str(), |bytes| {
                            m.bytes(bytes);
                        });
                    });
                }
                Datum::Bytea(b) => {
                    m.i32(b.len() as i32);
                    m.bytes(b);
                }
                Datum::Uuid(b) => {
                    m.i32(16);
                    m.bytes(b);
                }
                Datum::Inet(net) | Datum::Cidr(net) => {
                    // PostgreSQL `inet`/`cidr` send: family (2 = v4, 3 = v6),
                    // mask bits, is_cidr flag, address byte count, then the
                    // address bytes.
                    let nb = net.addr_len() as u8;
                    let family = if net.family() == 4 { 2u8 } else { 3u8 };
                    let is_cidr = u8::from(matches!(v, Datum::Cidr(_)));
                    m.i32(4 + i32::from(nb));
                    m.u8(family);
                    m.u8(net.bits());
                    m.u8(is_cidr);
                    m.u8(nb);
                    m.bytes(&net.addr()[..nb as usize]);
                }
                Datum::Macaddr(b) => {
                    m.i32(6);
                    m.bytes(b);
                }
                Datum::Macaddr8(b) => {
                    m.i32(8);
                    m.bytes(b);
                }
                Datum::Enum { label, .. } => {
                    // PostgreSQL `enum_send` emits the label text verbatim.
                    m.i32(label.len() as i32);
                    m.bytes(label.as_bytes());
                }
                Datum::Array { element, raw } => {
                    let elem_oid = element.element_oid();
                    let shape = crate::sql::array::shape(raw).expect("array datum invariant");
                    m.field(|m| {
                        m.i32(shape.dimension_count() as i32);
                        let has_null =
                            crate::sql::array::elements(raw, *element).any(|datum| datum.is_null());
                        m.i32(i32::from(has_null));
                        m.i32(elem_oid);
                        for index in 0..shape.dimension_count() {
                            m.i32(shape.dimension(index).unwrap() as i32);
                            m.i32(shape.lower_bound(index).unwrap());
                        }
                        for elem in crate::sql::array::elements(raw, *element) {
                            Self::encode_value_binary(m, &elem);
                        }
                    });
                }
                Datum::Int2Vector { raw, lower_bound } => {
                    let count = raw.len() / 2;
                    m.field(|m| {
                        m.i32(1);
                        m.i32(0);
                        m.i32(crate::sql::types::oid::INT2);
                        m.i32(count as i32);
                        m.i32(*lower_bound);
                        for bytes in raw.as_chunks::<2>().0 {
                            m.i32(2);
                            m.bytes(&i16::from_le_bytes(*bytes).to_be_bytes());
                        }
                    });
                }
                Datum::OidVector { raw, lower_bound } => {
                    let count = raw.len() / 4;
                    m.field(|m| {
                        m.i32(1);
                        m.i32(0);
                        m.i32(crate::sql::types::oid::OID);
                        m.i32(count as i32);
                        m.i32(*lower_bound);
                        for bytes in raw.as_chunks::<4>().0 {
                            m.i32(4);
                            m.bytes(&u32::from_le_bytes(*bytes).to_be_bytes());
                        }
                    });
                }
                Datum::Record(fields) | Datum::Composite { fields, .. } => {
                    // PostgreSQL's anonymous-record send format is a field
                    // count followed by each field's type OID and its ordinary
                    // binary field representation. Records are transient, but
                    // binary Bind results may still contain ROW(...) or a
                    // whole-row reference.
                    m.field(|m| {
                        m.i32(fields.len() as i32);
                        for field in *fields {
                            m.i32(field.type_oid);
                            Self::encode_value_binary(m, &field.value);
                        }
                    });
                }
                Datum::CompositeText { text, .. } => {
                    m.i32(text.len() as i32);
                    m.bytes(text.as_bytes());
                }
                Datum::Numeric(nm) => {
                    // PostgreSQL numeric binary: i16 ndigits, weight, sign,
                    // dscale, then ndigits big-endian base-10000 digits.
                    let nd = nm.ndigits();
                    m.i32((8 + nd * 2) as i32);
                    m.i16(nd as i16);
                    m.i16(nm.weight);
                    let sign_code: i16 = match nm.sign {
                        crate::sql::numeric::Sign::Pos => 0x0000,
                        crate::sql::numeric::Sign::Neg => 0x4000,
                        crate::sql::numeric::Sign::PosInf => -0x3000, // 0xD000
                        crate::sql::numeric::Sign::NegInf => -0x1000, // 0xF000
                        crate::sql::numeric::Sign::NaN => -0x4000,    // 0xC000
                    };
                    m.i16(sign_code);
                    m.i16(nm.dscale as i16);
                    for k in 0..nd {
                        m.bytes(&nm.digit(k).to_be_bytes());
                    }
                }
            }
        }
    }

    /// Writes the binary body of one built-in range from its canonical text.
    /// Range values retain text in `Datum`, so this intentionally parses only
    /// into stack state and never requires a connection or statement arena.
    fn encode_range_binary(message: &mut MsgOut, text: &str, kind: crate::sql::types::RangeKind) {
        message.field(|message| Self::encode_range_binary_body(message, text, kind));
    }

    fn encode_range_binary_body(
        message: &mut MsgOut,
        text: &str,
        kind: crate::sql::types::RangeKind,
    ) {
        let parsed = crate::sql::range::parse(text).expect("range datums are canonical");
        if parsed.empty {
            message.u8(0x01);
            return;
        }
        let mut flags = 0u8;
        if parsed.lower_inc {
            flags |= 0x02;
        }
        if parsed.upper_inc {
            flags |= 0x04;
        }
        if parsed.lower.is_none() {
            flags |= 0x08;
        }
        if parsed.upper.is_none() {
            flags |= 0x10;
        }
        message.u8(flags);
        if let Some(lower) = parsed.lower {
            Self::encode_range_bound_binary(message, lower, kind);
        }
        if let Some(upper) = parsed.upper {
            Self::encode_range_bound_binary(message, upper, kind);
        }
    }

    fn encode_multirange_binary(
        message: &mut MsgOut,
        text: &str,
        kind: crate::sql::types::RangeKind,
    ) {
        message.field(|message| {
            let count =
                crate::sql::range::component_count(text).expect("multirange datums are canonical");
            message.i32(count as i32);
            for component in
                crate::sql::range::components(text).expect("multirange datums are canonical")
            {
                message.field(|message| Self::encode_range_binary_body(message, component, kind));
            }
        });
    }

    fn encode_range_bound_binary(
        message: &mut MsgOut,
        text: &str,
        kind: crate::sql::types::RangeKind,
    ) {
        use crate::sql::types::RangeKind;
        match kind {
            RangeKind::Int4 => {
                let value = text.parse::<i32>().expect("canonical int4 range bound");
                message.i32(4);
                message.bytes(&value.to_be_bytes());
            }
            RangeKind::Int8 => {
                let value = text.parse::<i64>().expect("canonical int8 range bound");
                message.i32(8);
                message.bytes(&value.to_be_bytes());
            }
            RangeKind::Date => {
                let value =
                    crate::sql::datetime::parse_date(text).expect("canonical date range bound");
                message.i32(4);
                message.bytes(&value.to_be_bytes());
            }
            RangeKind::Ts | RangeKind::Tstz => {
                let value = crate::sql::datetime::parse_timestamp(text, kind == RangeKind::Tstz)
                    .expect("canonical timestamp range bound");
                message.i32(8);
                message.bytes(&value.to_be_bytes());
            }
            RangeKind::Num => Self::encode_numeric_text_binary(message, text),
        }
    }

    /// Encodes canonical numeric text without allocating an intermediate
    /// `Numeric`. Range text never uses exponent notation, so a bounded
    /// decimal scan is sufficient and preserves the displayed scale.
    fn encode_numeric_text_binary(message: &mut MsgOut, text: &str) {
        use crate::sql::numeric::{DEC_DIGITS, MAX_NDIGITS};
        let text = text.trim();
        if text.eq_ignore_ascii_case("nan") {
            message.i32(8);
            message.i16(0);
            message.i16(0);
            message.i16(-0x4000);
            message.i16(0);
            return;
        }
        let bytes = text.as_bytes();
        let (negative, digits) = match bytes.first() {
            Some(b'-') => (true, &bytes[1..]),
            Some(b'+') => (false, &bytes[1..]),
            _ => (false, bytes),
        };
        let mut decimal = [0u8; MAX_NDIGITS * DEC_DIGITS];
        let mut count = 0usize;
        let mut point = None;
        for byte in digits {
            if *byte == b'.' {
                point = Some(count);
            } else {
                debug_assert!(byte.is_ascii_digit(), "canonical numeric range bound");
                decimal[count] = *byte - b'0';
                count += 1;
            }
        }
        let integer_digits = point.unwrap_or(count);
        let scale = (count - integer_digits) as u16;
        let mut first = 0usize;
        while first < count && decimal[first] == 0 {
            first += 1;
        }
        let mut last = count;
        while last > first && decimal[last - 1] == 0 {
            last -= 1;
        }
        if first == last {
            message.i32(8);
            message.i16(0);
            message.i16(0);
            message.i16(0);
            message.i16(scale as i16);
            return;
        }
        let most_significant_weight = integer_digits as i64 - 1 - first as i64;
        let lead = (3 - most_significant_weight.rem_euclid(4)) % 4;
        let base_count = (lead as usize + last - first).div_ceil(DEC_DIGITS);
        debug_assert!(
            base_count <= MAX_NDIGITS,
            "canonical numeric range bound fits"
        );
        let mut base = [0i16; MAX_NDIGITS];
        for (index, slot) in base.iter_mut().enumerate().take(base_count) {
            let mut value = 0i16;
            for offset in 0..DEC_DIGITS {
                let decimal_index = index * DEC_DIGITS + offset;
                let digit = if decimal_index < lead as usize {
                    0
                } else {
                    decimal
                        .get(first + decimal_index - lead as usize)
                        .copied()
                        .unwrap_or(0)
                };
                value = value * 10 + digit as i16;
            }
            *slot = value;
        }
        let mut base_last = base_count;
        while base_last > 0 && base[base_last - 1] == 0 {
            base_last -= 1;
        }
        let base_weight = (most_significant_weight + lead).div_euclid(4);
        message.i32((8 + base_last * 2) as i32);
        message.i16(base_last as i16);
        message.i16(base_weight as i16);
        message.i16(if negative { 0x4000 } else { 0 });
        message.i16(scale as i16);
        for digit in &base[..base_last] {
            message.i16(*digit);
        }
    }

    /// Forwards complete pre-encoded wire messages.
    pub fn raw(&mut self, bytes: &[u8]) -> Result<(), WireFull> {
        let mut rest = bytes;
        while !rest.is_empty() {
            let length = rest
                .get(1..5)
                .and_then(|header| <[u8; 4]>::try_from(header).ok())
                .and_then(|header| usize::try_from(i32::from_be_bytes(header)).ok())
                .and_then(|length| length.checked_add(1))
                .filter(|length| *length >= 5 && *length <= rest.len())
                .expect("raw output holds complete backend messages");
            let (message, tail) = rest.split_at(length);
            self.control(|buffer| {
                if buffer.append(message) {
                    Ok(())
                } else {
                    Err(WireFull)
                }
            })?;
            rest = tail;
        }
        Ok(())
    }

    pub fn command_complete(&mut self, tag: &str) -> Result<(), WireFull> {
        if self.discard_query_output || self.suppress_command_complete {
            return Ok(());
        }
        let start = self.mark();
        self.control(|buffer| {
            let mut m = MsgOut::begin(buffer, wire::MSG_COMMAND_COMPLETE);
            m.cstr(tag);
            m.finish()
        })?;
        self.last_command_completion = Some((start, tag.len() + 6));
        Ok(())
    }

    /// The tag of the last CommandComplete, while it still ends the output.
    pub(crate) fn completion_tag(&self) -> Option<crate::util::StackStr<64>> {
        let (start, length) = self.last_command_completion?;
        if self.excess_since(start) != Some(length) {
            return None;
        }
        let frame = self.tail(length);
        core::str::from_utf8(&frame[5..length - 1])
            .ok()
            .map(crate::util::StackStr::from_str)
    }

    pub(crate) fn begin_command_completion_capture(&mut self) {
        self.last_command_completion = None;
    }

    pub(crate) fn take_command_completion_tag<'a>(
        &mut self,
        arena: &'a Arena,
        retain: bool,
    ) -> Result<Option<&'a str>, ArenaFull> {
        let Some((start, length)) = self.last_command_completion.take() else {
            return Ok(None);
        };
        assert_eq!(
            self.excess_since(start),
            Some(length),
            "command completion must finish statement output"
        );
        let retained = if retain {
            let frame = self.tail(length);
            let tag = core::str::from_utf8(&frame[5..length - 1])
                .expect("command tags originate as UTF-8 strings");
            arena.alloc_str(tag).map(Some)
        } else {
            Ok(None)
        };
        // Also remove success if the statement reservation cannot retain it.
        self.truncate_to(start);
        retained
    }

    /// Publishes an affected-row count alongside a DML command completion.
    /// Nested trigger execution consumes the count as typed execution state;
    /// clients still receive only PostgreSQL's ordinary command tag.
    pub(crate) fn command_complete_rows(&mut self, tag: &str, rows: u64) -> Result<(), WireFull> {
        self.command_complete(tag)?;
        self.set_affected_rows(rows);
        Ok(())
    }

    pub(crate) fn set_affected_rows(&mut self, rows: u64) {
        self.affected_rows = Some(rows);
    }

    pub(crate) fn take_affected_rows(&mut self) -> Option<u64> {
        self.affected_rows.take()
    }

    pub(crate) fn clear_affected_rows(&mut self) {
        self.affected_rows = None;
    }

    pub fn without_command_complete<T>(&mut self, operation: impl FnOnce(&mut Self) -> T) -> T {
        let prior = self.suppress_command_complete;
        self.suppress_command_complete = true;
        let result = operation(self);
        self.suppress_command_complete = prior;
        result
    }

    /// Runs an internal statement without exposing its row description, rows,
    /// or command tag. Diagnostic messages and errors still use the ordinary
    /// wire path.
    pub(crate) fn without_query_output<T>(&mut self, operation: impl FnOnce(&mut Self) -> T) -> T {
        let prior_discard = self.discard_query_output;
        let prior_command = self.suppress_command_complete;
        self.discard_query_output = true;
        self.suppress_command_complete = true;
        let result = operation(self);
        self.discard_query_output = prior_discard;
        self.suppress_command_complete = prior_command;
        result
    }

    pub fn empty_query_response(&mut self) -> Result<(), WireFull> {
        self.control(|buffer| MsgOut::begin(buffer, wire::MSG_EMPTY_QUERY_RESPONSE).finish())
    }

    /// NoticeResponse at NOTICE severity. Dropped when `client_min_messages`
    /// is above NOTICE (e.g. `warning`), matching PostgreSQL.
    pub fn notice<S: AsRef<str>>(&mut self, sqlstate: S, message: &str) -> Result<(), WireFull> {
        self.diagnostic(
            crate::sql::guc::MessageLevel::Notice,
            "NOTICE",
            sqlstate,
            message,
        )
    }

    /// NoticeResponse at WARNING severity. Dropped only when
    /// `client_min_messages` is above WARNING (i.e. `error`).
    pub fn warning<S: AsRef<str>>(&mut self, sqlstate: S, message: &str) -> Result<(), WireFull> {
        self.diagnostic(
            crate::sql::guc::MessageLevel::Warning,
            "WARNING",
            sqlstate,
            message,
        )
    }

    /// NoticeResponse at DEBUG severity, subject to `client_min_messages`.
    pub fn debug<S: AsRef<str>>(&mut self, sqlstate: S, message: &str) -> Result<(), WireFull> {
        self.diagnostic(
            crate::sql::guc::MessageLevel::Debug1,
            "DEBUG",
            sqlstate,
            message,
        )
    }

    /// NoticeResponse at LOG severity, subject to `client_min_messages`.
    pub fn log<S: AsRef<str>>(&mut self, sqlstate: S, message: &str) -> Result<(), WireFull> {
        self.diagnostic(crate::sql::guc::MessageLevel::Log, "LOG", sqlstate, message)
    }

    /// PostgreSQL sends INFO to the client regardless of `client_min_messages`.
    pub fn info<S: AsRef<str>>(&mut self, sqlstate: S, message: &str) -> Result<(), WireFull> {
        self.diagnostic_unfiltered("INFO", sqlstate, message)
    }

    /// Emits a NoticeResponse (NOTICE or WARNING severity) unless the session's
    /// `client_min_messages` threshold filters it out. Same field layout as
    /// errors.
    fn diagnostic<S: AsRef<str>>(
        &mut self,
        level: crate::sql::guc::MessageLevel,
        severity: &str,
        sqlstate: S,
        message: &str,
    ) -> Result<(), WireFull> {
        // The stashed detail belongs to this diagnostic even when the level
        // filter drops it — take it either way so it cannot leak forward.
        let diagnostic = crate::sql::eval::take_diagnostic();
        if !self.render_context().min_message_level.allows(level) {
            return Ok(());
        }
        self.write_notice_response(severity, sqlstate.as_ref(), message, diagnostic.as_ref())
    }

    fn diagnostic_unfiltered<S: AsRef<str>>(
        &mut self,
        severity: &str,
        sqlstate: S,
        message: &str,
    ) -> Result<(), WireFull> {
        let diagnostic = crate::sql::eval::take_diagnostic();
        self.write_notice_response(severity, sqlstate.as_ref(), message, diagnostic.as_ref())
    }

    fn write_notice_response(
        &mut self,
        severity: &str,
        sqlstate: &str,
        message: &str,
        diagnostic: Option<&crate::sql::eval::Diagnostic>,
    ) -> Result<(), WireFull> {
        self.control(|buffer| {
            let mut m = MsgOut::begin(buffer, wire::MSG_NOTICE_RESPONSE);
            m.u8(b'S');
            m.cstr(severity);
            m.u8(b'V');
            m.cstr(severity);
            m.u8(b'C');
            m.cstr(sqlstate);
            m.u8(b'M');
            m.cstr(message);
            if let Some(d) = diagnostic {
                if let Some(detail) = &d.detail {
                    m.u8(b'D');
                    m.cstr(detail.as_str());
                }
                if let Some(h) = &d.hint {
                    m.u8(b'H');
                    m.cstr(h.as_str());
                }
            }
            m.u8(0);
            m.finish()
        })
    }

    /// ErrorResponse with the fields every client expects: severity (twice,
    /// localized and not), SQLSTATE, and message.
    pub fn error<S: AsRef<str>>(&mut self, sqlstate: S, message: &str) -> Result<(), WireFull> {
        self.error_severity("ERROR", sqlstate.as_ref(), message)
    }

    pub fn fatal<S: AsRef<str>>(&mut self, sqlstate: S, message: &str) -> Result<(), WireFull> {
        self.error_severity("FATAL", sqlstate.as_ref(), message)
    }

    fn error_severity(
        &mut self,
        severity: &str,
        sqlstate: &str,
        message: &str,
    ) -> Result<(), WireFull> {
        let diagnostic = crate::sql::eval::take_diagnostic();
        self.control(|buffer| {
            let mut m = MsgOut::begin(buffer, wire::MSG_ERROR_RESPONSE);
            m.u8(b'S');
            m.cstr(severity);
            m.u8(b'V');
            m.cstr(severity);
            m.u8(b'C');
            m.cstr(sqlstate);
            m.u8(b'M');
            m.cstr(message);
            if let Some(d) = &diagnostic {
                if let Some(detail) = &d.detail {
                    m.u8(b'D');
                    m.cstr(detail.as_str());
                }
                if let Some(h) = &d.hint {
                    m.u8(b'H');
                    m.cstr(h.as_str());
                }
            }
            m.u8(0);
            m.finish()
        })
    }

    /// Reports a response that exceeded its output capacity. Unsent output
    /// after `mark` is discarded; output that already reached the client
    /// stays, and the error follows it as in a PostgreSQL result that fails
    /// midway. The message names the exhausted setting.
    pub fn replace_with_overflow_error(&mut self, mark: OutputMark) -> Result<(), WireFull> {
        if self.can_truncate_to(mark) {
            self.truncate_to(mark);
        }
        let exhaustion = self
            .output
            .as_mut()
            .and_then(|sink| sink.overflow.take_exhaustion())
            .unwrap_or(OverflowExhaustion::NoSpool);
        let mut message = crate::util::StackStr::<512>::new();
        let _ = core::fmt::Write::write_fmt(&mut message, format_args!("{exhaustion}"));
        self.error(
            crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
            message.as_str(),
        )
    }
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    fn message_kinds(mut bytes: &[u8], out: &mut [u8]) -> usize {
        let mut count = 0;
        while !bytes.is_empty() {
            let length = u32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize + 1;
            out[count] = bytes[0];
            count += 1;
            bytes = &bytes[length..];
        }
        count
    }

    #[test]
    fn overflow_output_keeps_messages_whole_and_marks_span_both_tiers() {
        use crate::mem::lease_pool::{LeasePool, Purpose};
        use crate::pg::overflow::ResultOverflow;
        let mut budget = crate::mem::Budget::new(1 << 20);
        let arena = crate::mem::arena::Arena::new(&mut budget, "tags", 64).unwrap();
        let spool = LeasePool::new(&mut budget, Purpose::RESULT_SPOOL, 192, 1).unwrap();
        // The private reserve must hold the named exhaustion error.
        let mut overflow = ResultOverflow::new(&mut budget, 256).unwrap();
        let mut send = super::FixedBuf::new(&mut budget, "send", 256).unwrap();
        let row = crate::sql::types::Datum::Text("abcdefghijklmnopqrstuvwx");
        crate::mem::guard::forbid_alloc(|| {
            let mut responder =
                super::Responder::new(&mut send).with_output(super::OutputSink::new(
                    &mut overflow,
                    Some(spool.clone()),
                    super::ClientWriter::None,
                ));
            for _ in 0..9 {
                responder.data_row(core::slice::from_ref(&row)).unwrap();
            }
            // Seven rows fill the send buffer; two wait in the shared spool.
            assert_eq!(responder.buffer.len(), 245);
            let mark = responder.mark();
            responder.data_row(core::slice::from_ref(&row)).unwrap();
            responder.notice("00000", "rolled back").unwrap();
            responder.truncate_to(mark);
            assert_eq!(responder.mark(), mark);
            responder.command_complete("SELECT 3").unwrap();
            assert_eq!(responder.completion_tag().unwrap().as_str(), "SELECT 3");
            let tag = responder
                .take_command_completion_tag(&arena, true)
                .unwrap()
                .unwrap();
            responder.command_complete(tag).unwrap();
            // Rows stop at the spool limit; terminal messages use the reserve.
            let failed = loop {
                let before = responder.mark();
                if responder.data_row(core::slice::from_ref(&row)).is_err() {
                    break before;
                }
            };
            assert_eq!(responder.mark(), failed);
            responder.replace_with_overflow_error(failed).unwrap();
            responder.ready_for_query(b'I').unwrap();
        });
        let mut kinds = [0u8; 32];
        let mut count = message_kinds(send.readable(), &mut kinds);
        send.clear();
        while overflow.refill(&mut send) {
            count += message_kinds(send.readable(), &mut kinds[count..]);
            send.clear();
        }
        let kinds = &kinds[..count];
        assert!(
            kinds.starts_with(b"DDDDDDDDDCD") && kinds.ends_with(b"EZ"),
            "{}",
            String::from_utf8_lossy(kinds)
        );
        assert!(kinds[10..count - 2].iter().all(|kind| *kind == b'D'));
        assert!(overflow.is_empty());
    }

    #[test]
    fn promotion_requests_a_stream_only_before_effects_and_sent_bytes() {
        use crate::mem::lease_pool::{LeasePool, Purpose};
        use crate::pg::overflow::ResultOverflow;
        let mut budget = crate::mem::Budget::new(1 << 20);
        let spool = LeasePool::new(&mut budget, Purpose::RESULT_SPOOL, 4096, 1).unwrap();
        let mut overflow = ResultOverflow::new(&mut budget, 256).unwrap();
        let mut send = super::FixedBuf::new(&mut budget, "send", 256).unwrap();
        let row = crate::sql::types::Datum::Text("abcdefghijklmnopqrstuvwx");
        crate::mem::guard::forbid_alloc(|| {
            let mut responder = super::Responder::new(&mut send).with_output(
                super::OutputSink::new(
                    &mut overflow,
                    Some(spool.clone()),
                    super::ClientWriter::None,
                )
                .allow_promotion(),
            );
            responder.arm_promotion();
            let mark = responder.mark();
            let full = loop {
                if let Err(full) = responder.data_row(core::slice::from_ref(&row)) {
                    break full;
                }
            };
            assert_eq!(full, WireFull);
            // A discarded attempt queues nothing more, not even errors.
            assert!(responder.error("XX000", "after the request").is_err());
            assert_eq!(responder.finish_promotion(), (true, None));
            responder.truncate_to(mark);

            // An effect before the first overflow keeps the statement on the spool.
            responder.arm_promotion();
            crate::sql::query::protect_statement_effects();
            for _ in 0..12 {
                responder.data_row(core::slice::from_ref(&row)).unwrap();
            }
            assert_eq!(
                responder.finish_promotion(),
                (false, Some(super::SpoolReason::Effects))
            );
            responder.truncate_to(mark);

            responder.decline_promotion(super::SpoolReason::Shape);
            for _ in 0..12 {
                responder.data_row(core::slice::from_ref(&row)).unwrap();
            }
            assert_eq!(
                responder.finish_promotion(),
                (false, Some(super::SpoolReason::Shape))
            );
            assert!(responder.promotion_allowed());
        });
    }

    #[test]
    #[cfg(unix)]
    fn overflow_output_writes_without_blocking_and_preserves_order() {
        use crate::mem::lease_pool::{LeasePool, Purpose};
        use crate::pg::overflow::ResultOverflow;
        use std::io::Read;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        let (writer, mut reader) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        // Fill the socket so the first writes accept nothing.
        let filler = [0u8; 4096];
        let mut filled = 0usize;
        loop {
            let n =
                unsafe { libc::write(writer.as_raw_fd(), filler.as_ptr().cast(), filler.len()) };
            if n <= 0 {
                break;
            }
            filled += n as usize;
        }
        let mut budget = crate::mem::Budget::new(1 << 20);
        let spool = LeasePool::new(&mut budget, Purpose::RESULT_SPOOL, 1 << 16, 1).unwrap();
        let mut overflow = ResultOverflow::new(&mut budget, 64).unwrap();
        let mut send = super::FixedBuf::new(&mut budget, "send", 64).unwrap();
        let rows = 200u32;
        crate::mem::guard::forbid_alloc(|| {
            let mut responder =
                super::Responder::new(&mut send).with_output(super::OutputSink::new(
                    &mut overflow,
                    Some(spool.clone()),
                    super::ClientWriter::Fd(writer.as_raw_fd()),
                ));
            for value in 0..rows / 2 {
                responder
                    .data_row(&[crate::sql::types::Datum::Int4(1000 + value as i32)])
                    .unwrap();
            }
            assert!(!responder.transport_failed());
        });
        assert_eq!(overflow.written(), 0, "a full socket accepts nothing");
        let mut sink = vec![0u8; filled];
        reader.read_exact(&mut sink).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mut responder =
                super::Responder::new(&mut send).with_output(super::OutputSink::new(
                    &mut overflow,
                    Some(spool.clone()),
                    super::ClientWriter::Fd(writer.as_raw_fd()),
                ));
            for value in rows / 2..rows {
                responder
                    .data_row(&[crate::sql::types::Datum::Int4(1000 + value as i32)])
                    .unwrap();
            }
        });
        assert!(overflow.written() > 0);
        let total = (rows as usize) * 15;
        let queued = send.len() + overflow.len();
        assert_eq!(overflow.written() as usize + queued, total);
        let mut received = vec![0u8; overflow.written() as usize];
        reader.read_exact(&mut received).unwrap();
        received.extend_from_slice(send.readable());
        send.clear();
        while overflow.refill(&mut send) {
            received.extend_from_slice(send.readable());
            send.clear();
        }
        for (index, frame) in received.chunks(15).enumerate() {
            assert_eq!(frame[0], b'D');
            assert_eq!(&frame[11..15], (1000 + index).to_string().as_bytes());
        }
    }

    use super::*;
    use crate::mem::Budget;
    use crate::mem::arena::Arena;
    use crate::sql::types::{RangeKind, RecordField, oid};

    #[test]
    fn event_trigger_completion_capture_preserves_notices_and_compaction() {
        let mut budget = Budget::new(8192);
        let mut buffer = FixedBuf::new(&mut budget, "completion capture", 4096).unwrap();
        let arena = Arena::new(&mut budget, "completion tags", 1024).unwrap();
        buffer.append(b"prefix");
        buffer.consume(3);
        let mut response = Responder::new(&mut buffer);
        crate::mem::guard::forbid_alloc(|| {
            response.begin_command_completion_capture();
            response.notice("00000", "before completion").unwrap();
            let expected = response.buffer.readable().len();
            response.command_complete("CREATE TABLE").unwrap();
            let tag = response
                .take_command_completion_tag(&arena, true)
                .unwrap()
                .unwrap();
            assert_eq!(tag, "CREATE TABLE");
            assert_eq!(response.buffer.len(), expected);
            response.notice("00000", "hook notice").unwrap();
            response.command_complete(tag).unwrap();
            let bytes = response.buffer.readable();
            assert!(bytes.ends_with(b"CREATE TABLE\0"));
            assert!(
                bytes
                    .windows(b"hook notice".len())
                    .any(|part| part == b"hook notice")
            );
        });
    }

    #[test]
    fn event_trigger_completion_exhaustion_never_leaves_success() {
        let mut budget = Budget::new(8192);
        let mut buffer = FixedBuf::new(&mut budget, "completion exhaustion", 4096).unwrap();
        let arena = Arena::new(&mut budget, "completion tags", 1).unwrap();
        let mut response = Responder::new(&mut buffer);
        crate::mem::guard::forbid_alloc(|| {
            response.begin_command_completion_capture();
            response
                .notice("00000", "notice retained on failure")
                .unwrap();
            let expected = response.buffer.len();
            response.command_complete("CREATE TABLE").unwrap();
            assert!(response.take_command_completion_tag(&arena, true).is_err());
            assert_eq!(response.buffer.len(), expected);
            response.begin_command_completion_capture();
            response.command_complete("DROP TABLE").unwrap();
            assert!(
                response
                    .take_command_completion_tag(&arena, false)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(response.buffer.len(), expected);
        });
    }

    #[test]
    fn select_one_wire_bytes() {
        let mut budget = Budget::new(1 << 16);
        let mut buffer = FixedBuf::new(&mut budget, "test", 1024).unwrap();
        let mut r = Responder::new(&mut buffer);
        r.row_description(&[ColDesc::new("?column?", oid::INT4, 4)])
            .unwrap();
        r.data_row(&[Datum::Int4(1)]).unwrap();
        r.command_complete("SELECT 1").unwrap();
        r.ready_for_query(b'I').unwrap();

        let bytes = buffer.readable();
        // RowDescription: T, len, 1 column
        assert_eq!(bytes[0], b'T');
        // DataRow holds the text "1"
        let d = bytes.iter().position(|&b| b == b'D').unwrap();
        assert_eq!(&bytes[d + 5..d + 7], &[0, 1]); // one column
        assert_eq!(&bytes[d + 7..d + 11], &1i32.to_be_bytes()); // length 1
        assert_eq!(bytes[d + 11], b'1');
        // Trailer: ready for query, idle
        assert_eq!(&bytes[bytes.len() - 6..], &[b'Z', 0, 0, 0, 5, b'I']);
    }

    #[test]
    fn error_response_fields() {
        let mut budget = Budget::new(1 << 16);
        let mut buffer = FixedBuf::new(&mut budget, "test", 256).unwrap();
        let mut r = Responder::new(&mut buffer);
        r.error("42601", "syntax error").unwrap();
        let bytes = buffer.readable();
        assert_eq!(bytes[0], b'E');
        let text = core::str::from_utf8(&bytes[5..]).unwrap();
        assert!(text.contains("42601"));
        assert!(text.contains("syntax error"));
    }

    #[test]
    fn diagnostics_preserve_absent_and_explicitly_empty_detail_fields() {
        let mut budget = Budget::new(256);
        let mut buffer = FixedBuf::new(&mut budget, "optional diagnostic fields", 256).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            for empty_detail in [false, true] {
                for notice in [false, true] {
                    buffer.clear();
                    crate::sql::eval::stash_diagnostic(
                        empty_detail.then(crate::util::StackStr::new),
                        Some(crate::util::StackStr::from_str("use an authorized role")),
                    );
                    let mut responder = Responder::new(&mut buffer);
                    if notice {
                        responder.notice("42501", "denied").unwrap();
                    } else {
                        responder.error("42501", "denied").unwrap();
                    }
                    let expected: &[u8] = match (notice, empty_detail) {
                        (false, false) => {
                            b"SERROR\0VERROR\0C42501\0Mdenied\0Huse an authorized role\0\0"
                        }
                        (false, true) => {
                            b"SERROR\0VERROR\0C42501\0Mdenied\0D\0Huse an authorized role\0\0"
                        }
                        (true, false) => {
                            b"SNOTICE\0VNOTICE\0C42501\0Mdenied\0Huse an authorized role\0\0"
                        }
                        (true, true) => {
                            b"SNOTICE\0VNOTICE\0C42501\0Mdenied\0D\0Huse an authorized role\0\0"
                        }
                    };
                    assert_eq!(&buffer.readable()[5..], expected);
                    assert!(crate::sql::eval::take_diagnostic().is_none());
                }
            }
        });
    }

    #[test]
    fn copy_both_response_matches_postgresqls_zero_column_format() {
        let mut budget = Budget::new(1 << 16);
        let mut buffer = FixedBuf::new(&mut budget, "test", 256).unwrap();
        Responder::new(&mut buffer).copy_both_response().unwrap();
        assert_eq!(buffer.readable(), &[b'W', 0, 0, 0, 7, 0, 0, 0]);
    }

    #[test]
    fn md5_authentication_challenge_matches_the_postgresql_wire_frame() {
        let mut budget = Budget::new(1 << 16);
        let mut buffer = FixedBuf::new(&mut budget, "test", 256).unwrap();
        Responder::new(&mut buffer)
            .auth_md5_password(&[1, 2, 3, 4])
            .unwrap();
        assert_eq!(
            buffer.readable(),
            &[b'R', 0, 0, 0, 12, 0, 0, 0, 5, 1, 2, 3, 4]
        );
    }

    #[test]
    fn binary_record_uses_field_oids_and_binary_field_values() {
        let fields = [
            RecordField {
                name: "id",
                type_oid: oid::INT4,
                value: Datum::Int4(42),
            },
            RecordField {
                name: "note",
                type_oid: oid::TEXT,
                value: Datum::Null,
            },
        ];
        let mut budget = Budget::new(1 << 16);
        let mut buffer = FixedBuf::new(&mut budget, "test", 256).unwrap();
        let mut message = MsgOut::begin(&mut buffer, b'd');
        Responder::encode_value_binary(&mut message, &Datum::Record(&fields));
        message.finish().unwrap();
        assert_eq!(
            buffer.readable(),
            &[
                b'd', 0, 0, 0, 32, // CopyData outer frame.
                0, 0, 0, 24, // record body length.
                0, 0, 0, 2, // field count.
                0, 0, 0, 23, // int4 OID.
                0, 0, 0, 4, 0, 0, 0, 42, // int4 value.
                0, 0, 0, 25, // text OID.
                0xff, 0xff, 0xff, 0xff, // NULL field.
            ]
        );
    }

    #[test]
    fn internal_char_escapes_non_utf8_text_and_keeps_binary_raw() {
        let mut budget = Budget::new(1 << 16);
        let mut buffer = FixedBuf::new(&mut budget, "raw char", 64).unwrap();
        let mut text = MsgOut::begin(&mut buffer, b'd');
        Responder::encode_value_text(
            &mut text,
            &Datum::Char(0xff),
            crate::sql::guc::RenderContext::default(),
        );
        text.finish().unwrap();
        assert_eq!(
            buffer.readable(),
            &[b'd', 0, 0, 0, 12, 0, 0, 0, 4, b'\\', b'3', b'7', b'7']
        );

        buffer.clear();
        let mut binary = MsgOut::begin(&mut buffer, b'd');
        Responder::encode_value_binary(&mut binary, &Datum::Char(0xff));
        binary.finish().unwrap();
        assert_eq!(buffer.readable(), &[b'd', 0, 0, 0, 9, 0, 0, 0, 1, 0xff]);

        // PostgreSQL's charout() treats the zero sentinel as an empty C
        // string, while charsend() still transmits its one-byte representation.
        buffer.clear();
        let mut text = MsgOut::begin(&mut buffer, b'd');
        Responder::encode_value_text(
            &mut text,
            &Datum::Char(0),
            crate::sql::guc::RenderContext::default(),
        );
        text.finish().unwrap();
        assert_eq!(buffer.readable(), &[b'd', 0, 0, 0, 8, 0, 0, 0, 0]);

        buffer.clear();
        let mut binary = MsgOut::begin(&mut buffer, b'd');
        Responder::encode_value_binary(&mut binary, &Datum::Char(0));
        binary.finish().unwrap();
        assert_eq!(buffer.readable(), &[b'd', 0, 0, 0, 9, 0, 0, 0, 1, 0]);
    }

    #[test]
    fn binary_transaction_identity_results_use_postgresql_network_format() {
        let mut arena_budget = Budget::new(4096);
        let arena = Arena::new(&mut arena_budget, "transaction identity result", 1024).unwrap();
        let snapshot = crate::sql::snapshot::Snapshot::from_text("10:20:11,14", &arena).unwrap();
        let mut budget = Budget::new(4096);
        let mut buffer = FixedBuf::new(&mut budget, "transaction identity result", 256).unwrap();
        let mut message = MsgOut::begin(&mut buffer, b'd');
        Responder::encode_value_binary(&mut message, &Datum::Xid8(u64::MAX));
        Responder::encode_value_binary(
            &mut message,
            &Datum::Snapshot {
                value: snapshot,
                legacy: false,
            },
        );
        message.finish().unwrap();
        let bytes = buffer.readable();
        assert_eq!(&bytes[5..9], &8i32.to_be_bytes());
        assert_eq!(&bytes[9..17], &u64::MAX.to_be_bytes());
        assert_eq!(&bytes[17..21], &36i32.to_be_bytes());
        assert_eq!(&bytes[21..], snapshot.raw());
    }

    #[test]
    fn binary_tuple_and_command_identity_results_use_postgresql_network_format() {
        let mut budget = Budget::new(128);
        let mut buffer = FixedBuf::new(&mut budget, "low-level identity result", 64).unwrap();
        let mut message = MsgOut::begin(&mut buffer, b'd');
        Responder::encode_value_binary(
            &mut message,
            &Datum::Tid(crate::sql::types::Tid {
                block: 0x0102_0304,
                offset: 0x0506,
            }),
        );
        Responder::encode_value_binary(&mut message, &Datum::Cid(0x0708_090a));
        message.finish().unwrap();
        assert_eq!(&buffer.readable()[5..9], &6i32.to_be_bytes());
        assert_eq!(&buffer.readable()[9..15], &[1, 2, 3, 4, 5, 6]);
        assert_eq!(&buffer.readable()[15..19], &4i32.to_be_bytes());
        assert_eq!(&buffer.readable()[19..23], &[7, 8, 9, 10]);
    }

    #[test]
    fn binary_money_results_are_exact_signed_big_endian_cents() {
        let mut budget = Budget::new(128);
        let mut buffer = FixedBuf::new(&mut budget, "money result", 32).unwrap();
        let mut message = MsgOut::begin(&mut buffer, b'd');
        Responder::encode_value_binary(&mut message, &Datum::Money(-123_456));
        message.finish().unwrap();
        assert_eq!(&buffer.readable()[..9], &[b'd', 0, 0, 0, 16, 0, 0, 0, 8]);
        assert_eq!(&buffer.readable()[9..], &(-123_456_i64).to_be_bytes());
    }

    #[test]
    fn internal_char_array_text_preserves_raw_elements_and_bounds() {
        let mut arena_budget = Budget::new(1 << 16);
        let arena = Arena::new(&mut arena_budget, "raw char array", 1 << 12).unwrap();
        let raw = crate::sql::array::build_shaped(
            &[Datum::Char(0xff), Datum::Char(b'{')],
            crate::sql::array::Shape::new(&[2], &[0]).unwrap(),
            &arena,
        )
        .unwrap();
        let value = Datum::Array {
            element: crate::sql::types::ArrElem::Char,
            raw,
        };
        let expected = b"[0:1]={\"\\\\377\",\"{\"}";

        let mut budget = Budget::new(1 << 16);
        let mut buffer = FixedBuf::new(&mut budget, "raw char array", 64).unwrap();
        let mut message = MsgOut::begin(&mut buffer, b'd');
        Responder::encode_value_text(
            &mut message,
            &value,
            crate::sql::guc::RenderContext::default(),
        );
        message.finish().unwrap();
        assert_eq!(
            &buffer.readable()[5..9],
            &(expected.len() as i32).to_be_bytes()
        );
        assert_eq!(&buffer.readable()[9..], expected);
        assert_eq!(
            Responder::datum_wire_text(&value, crate::sql::guc::RenderContext::default(), &arena)
                .unwrap(),
            Some(expected.as_slice())
        );
    }

    #[test]
    fn internal_char_record_text_preserves_the_raw_field_byte() {
        let fields = [RecordField {
            name: "kind",
            type_oid: oid::CHAR,
            value: Datum::Char(0xff),
        }];
        let value = Datum::Record(&fields);
        let mut budget = Budget::new(1 << 16);
        let mut buffer = FixedBuf::new(&mut budget, "raw char record", 64).unwrap();
        let mut message = MsgOut::begin(&mut buffer, b'd');
        Responder::encode_value_text(
            &mut message,
            &value,
            crate::sql::guc::RenderContext::default(),
        );
        message.finish().unwrap();
        assert_eq!(buffer.readable(), b"d\0\0\0\x11\0\0\0\x09(\"\\\\377\")");
    }

    #[test]
    fn record_text_doubles_quotes_and_backslashes() {
        let fields = [RecordField {
            name: "value",
            type_oid: oid::TEXT,
            value: Datum::Text("quote\"and\\slash"),
        }];
        let value = Datum::Record(&fields);
        let mut budget = Budget::new(1 << 16);
        let mut buffer = FixedBuf::new(&mut budget, "record escapes", 64).unwrap();
        let mut message = MsgOut::begin(&mut buffer, b'd');
        Responder::encode_value_text(
            &mut message,
            &value,
            crate::sql::guc::RenderContext::default(),
        );
        message.finish().unwrap();
        let expected = b"(\"quote\"\"and\\\\slash\")";
        assert_eq!(
            &buffer.readable()[5..9],
            &(expected.len() as i32).to_be_bytes()
        );
        assert_eq!(&buffer.readable()[9..], expected);
    }

    #[test]
    fn binary_array_emits_dimensions_and_lower_bounds() {
        let mut arena_budget = Budget::new(1 << 16);
        let arena = Arena::new(&mut arena_budget, "binary array", 1 << 12).unwrap();
        let shape = crate::sql::array::Shape::new(&[2, 2], &[2, 4]).unwrap();
        let raw = crate::sql::array::build_shaped(
            &[
                Datum::Int4(1),
                Datum::Int4(2),
                Datum::Int4(3),
                Datum::Int4(4),
            ],
            shape,
            &arena,
        )
        .unwrap();
        let mut budget = Budget::new(1 << 16);
        let mut buffer = FixedBuf::new(&mut budget, "test", 256).unwrap();
        let mut message = MsgOut::begin(&mut buffer, b'd');
        Responder::encode_value_binary(
            &mut message,
            &Datum::Array {
                element: crate::sql::types::ArrElem::Int4,
                raw,
            },
        );
        message.finish().unwrap();
        assert_eq!(
            &buffer.readable()[9..37],
            &[
                0, 0, 0, 2, // dimensions
                0, 0, 0, 0, // no nulls
                0, 0, 0, 23, // int4 OID
                0, 0, 0, 2, 0, 0, 0, 2, // first dimension
                0, 0, 0, 2, 0, 0, 0, 4, // second dimension
            ]
        );
    }

    #[test]
    fn binary_array_output_crosses_the_former_inline_element_limit() {
        const ELEMENTS: usize = 1_100;
        let mut arena_budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut arena_budget, "wide binary array", 1 << 19).unwrap();
        let values = crate::sql::array::alloc_items(&arena, ELEMENTS).unwrap();
        for (index, value) in values.iter_mut().enumerate() {
            *value = Datum::Int4(index as i32 + 1);
        }
        let raw = crate::sql::array::build(values, &arena).unwrap();
        let datum = Datum::Array {
            element: crate::sql::types::ArrElem::Int4,
            raw,
        };

        let mut budget = Budget::new(1 << 16);
        let mut buffer = FixedBuf::new(&mut budget, "wide binary result", 16 << 10).unwrap();
        let mut message = MsgOut::begin(&mut buffer, b'd');
        Responder::encode_value_binary(&mut message, &datum);
        message.finish().unwrap();
        let encoded = buffer.readable();
        assert_eq!(
            i32::from_be_bytes(encoded[5..9].try_into().unwrap()),
            (20 + ELEMENTS * 8) as i32
        );
        assert_eq!(
            i32::from_be_bytes(encoded[21..25].try_into().unwrap()),
            ELEMENTS as i32
        );
        let last = 29 + (ELEMENTS - 1) * 8;
        assert_eq!(
            i32::from_be_bytes(encoded[last..last + 4].try_into().unwrap()),
            4
        );
        assert_eq!(
            i32::from_be_bytes(encoded[last + 4..last + 8].try_into().unwrap()),
            ELEMENTS as i32
        );
    }

    #[test]
    fn binary_range_uses_flags_and_typed_bounds() {
        let mut budget = Budget::new(1 << 16);
        let mut buffer = FixedBuf::new(&mut budget, "test", 256).unwrap();
        let mut message = MsgOut::begin(&mut buffer, b'd');
        Responder::encode_value_binary(
            &mut message,
            &Datum::Range {
                text: "[1,5)",
                kind: RangeKind::Int4,
            },
        );
        message.finish().unwrap();
        assert_eq!(
            &buffer.readable()[5..],
            &[
                0, 0, 0, 17,   // range body length.
                0x02, // lower inclusive.
                0, 0, 0, 4, 0, 0, 0, 1, // lower int4.
                0, 0, 0, 4, 0, 0, 0, 5, // upper int4.
            ]
        );
    }

    #[test]
    fn binary_multirange_nests_binary_ranges() {
        let mut budget = Budget::new(1 << 16);
        let mut buffer = FixedBuf::new(&mut budget, "test", 256).unwrap();
        let mut message = MsgOut::begin(&mut buffer, b'd');
        Responder::encode_value_binary(
            &mut message,
            &Datum::Multirange {
                text: "{[1,3),[5,7)}",
                kind: RangeKind::Int4,
            },
        );
        message.finish().unwrap();
        assert_eq!(
            &buffer.readable()[5..],
            &[
                0, 0, 0, 46, // multirange body length.
                0, 0, 0, 2, // component count.
                0, 0, 0, 17, 0x02, 0, 0, 0, 4, 0, 0, 0, 1, 0, 0, 0, 4, 0, 0, 0,
                3, // first range.
                0, 0, 0, 17, 0x02, 0, 0, 0, 4, 0, 0, 0, 5, 0, 0, 0, 4, 0, 0, 0,
                7, // second range.
            ]
        );
    }

    #[test]
    fn binary_multirange_output_crosses_the_former_component_limit() {
        use core::fmt::Write;

        let mut text = String::from("{");
        for index in 0..128 {
            if index != 0 {
                text.push(',');
            }
            write!(text, "[{},{})", index * 3, index * 3 + 1).unwrap();
        }
        text.push('}');
        let mut budget = Budget::new(1 << 16);
        let mut buffer = FixedBuf::new(&mut budget, "test", 8 << 10).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mut message = MsgOut::begin(&mut buffer, b'd');
            Responder::encode_value_binary(
                &mut message,
                &Datum::Multirange {
                    text: &text,
                    kind: RangeKind::Int4,
                },
            );
            message.finish().unwrap();
        });
        assert_eq!(
            i32::from_be_bytes(buffer.readable()[9..13].try_into().unwrap()),
            128
        );
    }
}
