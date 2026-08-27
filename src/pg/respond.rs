//! Backend message construction: the typed layer between the engine and
//! the raw send buffer.

use crate::mem::buffer::FixedBuf;
use crate::sql::ast::ExplainSerialize;
use crate::sql::types::{ColDesc, Datum};
use crate::stack_format;

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

/// Where a full send buffer drains (blocking) so arbitrarily large results
/// stream instead of failing with 54000: nowhere, a raw fd (plaintext), or the
/// connection's TLS session (the plaintext is encrypted, then written to the
/// blocking socket). The borrows share the Responder's lifetime — all are
/// disjoint fields of the same connection, live for the streamed query.
pub enum FlushSink<'b> {
    None,
    Fd(i32),
    Tls {
        session: &'b mut crate::pg::tls::ServerSession,
        socket: &'b mut std::net::TcpStream,
    },
}

pub struct Responder<'b> {
    pub buffer: &'b mut FixedBuf,
    /// SQL cursor materialization captures both representations during one
    /// evaluation so FETCH can honor its own result-format codes.
    alternate_buffer: Option<&'b mut FixedBuf>,
    alternate_formats: ResultFmt,
    /// Extended-protocol Execute must not resend RowDescription (the
    /// client got it from Describe).
    suppress_row_description: bool,
    /// Per-column result format requested by Bind.
    formats: ResultFmt,
    /// Where a full send buffer drains so large results stream. The socket is
    /// put in blocking mode by the caller for the duration.
    flush: FlushSink<'b>,
    /// Session value-rendering settings (DateStyle, time zone).
    render: crate::sql::guc::RenderContext,
    /// EXPLAIN ANALYZE executes through the ordinary executor but suppresses
    /// its row description, rows, and command tag. Errors still propagate.
    discard_query_output: bool,
    /// Composite utility commands execute ordinary DROP helpers but publish
    /// one command tag of their own.
    suppress_command_complete: bool,
    /// Typed affected-row count for an internally executed DML command. This
    /// never derives state from a serialized command tag.
    affected_rows: Option<u64>,
    discarded_rows: u64,
    discard_serialize: ExplainSerialize,
    serialized_bytes: u64,
    serialization_micros: u64,
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

fn text_value_len(value: &Datum, render: crate::sql::guc::RenderContext) -> usize {
    match value {
        Datum::Null => 0,
        Datum::Text(text)
        | Datum::Bpchar(text)
        | Datum::Regtype { name: text, .. }
        | Datum::RegObject { name: text, .. } => text.len(),
        Datum::Bytea(bytes) if render.bytea_escape => bytes
            .iter()
            .map(|byte| match byte {
                b'\\' => 2,
                0x20..=0x7e => 1,
                _ => 4,
            })
            .sum(),
        Datum::Bytea(bytes) => 2usize.saturating_add(bytes.len().saturating_mul(2)),
        Datum::Numeric(number) => display_len(number),
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
        Datum::Bool(_) => 1,
        Datum::Int2(_) => 2,
        Datum::Int4(_)
        | Datum::Oid(_)
        | Datum::Regtype { .. }
        | Datum::RegObject { .. }
        | Datum::Date(_)
        | Datum::Float4(_) => 4,
        Datum::Int8(_)
        | Datum::Timestamp(_)
        | Datum::Timestamptz(_)
        | Datum::Time(_)
        | Datum::Float8(_)
        | Datum::Macaddr8(_) => 8,
        Datum::Macaddr(_) => 6,
        Datum::Timetz(..) => 12,
        Datum::Interval(_) | Datum::Uuid(_) => 16,
        Datum::Text(text) | Datum::Bpchar(text) => text.len(),
        Datum::Bytea(bytes) => bytes.len(),
        Datum::Json { text, jsonb } => text.len().saturating_add(usize::from(*jsonb)),
        Datum::Range { text, .. } | Datum::Multirange { text, .. } => text.len(),
        Datum::Bit { bits, .. } => 4usize.saturating_add(bits.len().div_ceil(8)),
        Datum::Inet(network) | Datum::Cidr(network) => 4usize.saturating_add(network.addr_len()),
        Datum::Enum { label, .. } => label.len(),
        Datum::Numeric(number) => 8usize.saturating_add(number.ndigits().saturating_mul(2)),
        Datum::Record(_) | Datum::Composite { .. } | Datum::CompositeText { .. } => {
            display_len(value)
        }
        Datum::Int2Vector(raw) => {
            let count = raw.len() / 2;
            12usize
                .saturating_add(usize::from(count != 0).saturating_mul(8))
                .saturating_add(count.saturating_mul(6))
        }
        Datum::OidVector(raw) => {
            let count = raw.len() / 4;
            12usize
                .saturating_add(usize::from(count != 0).saturating_mul(8))
                .saturating_add(count.saturating_mul(8))
        }
        Datum::Array { element, raw } => {
            let shape = crate::sql::array::shape(raw).expect("array datum invariant");
            let count = shape.element_count();
            let mut bytes = 12usize.saturating_add(shape.dimension_count().saturating_mul(8));
            for index in 0..count {
                let item = crate::sql::array::get(raw, *element, index).unwrap_or(Datum::Null);
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
            alternate_buffer: None,
            alternate_formats: ResultFmt::ALL_TEXT,
            suppress_row_description: false,
            formats: ResultFmt::ALL_TEXT,
            flush: FlushSink::None,
            render: crate::sql::guc::RenderContext::default(),
            discard_query_output: false,
            suppress_command_complete: false,
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
            alternate_buffer: None,
            alternate_formats: ResultFmt::ALL_TEXT,
            suppress_row_description: true,
            formats,
            flush: FlushSink::None,
            render: crate::sql::guc::RenderContext::default(),
            discard_query_output: false,
            suppress_command_complete: false,
            affected_rows: None,
            discarded_rows: 0,
            discard_serialize: ExplainSerialize::None,
            serialized_bytes: 0,
            serialization_micros: 0,
        }
    }

    /// Captures text and binary SQL-cursor rows from one execution.
    pub fn for_cursor(text: &'b mut FixedBuf, binary: &'b mut FixedBuf) -> Self {
        Self {
            buffer: text,
            alternate_buffer: Some(binary),
            alternate_formats: ResultFmt::ALL_BINARY,
            suppress_row_description: false,
            formats: ResultFmt::ALL_TEXT,
            flush: FlushSink::None,
            render: crate::sql::guc::RenderContext::default(),
            discard_query_output: false,
            suppress_command_complete: false,
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
            alternate_buffer: None,
            alternate_formats: ResultFmt::ALL_TEXT,
            suppress_row_description: false,
            formats,
            flush: FlushSink::None,
            render: crate::sql::guc::RenderContext::default(),
            discard_query_output: false,
            suppress_command_complete: false,
            affected_rows: None,
            discarded_rows: 0,
            discard_serialize: ExplainSerialize::None,
            serialized_bytes: 0,
            serialization_micros: 0,
        }
    }

    /// Enables streaming: a full buffer drains to `fd` and the message is
    /// retried. `fd` must be a blocking socket for the drain to complete.
    pub fn with_flush(mut self, fd: i32) -> Self {
        self.flush = FlushSink::Fd(fd);
        self
    }

    /// Enables streaming over a TLS session: a full buffer is encrypted and
    /// written to the (blocking) socket, then the message is retried.
    pub fn with_flush_tls(
        mut self,
        session: &'b mut crate::pg::tls::ServerSession,
        socket: &'b mut std::net::TcpStream,
    ) -> Self {
        self.flush = FlushSink::Tls { session, socket };
        self
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

    /// Drains the whole send buffer to the flush sink, blocking. Returns
    /// whether it fully drained (false = the sink errored or there is none).
    fn drain(&mut self) -> bool {
        match &mut self.flush {
            FlushSink::None => false,
            FlushSink::Fd(fd) => {
                let fd = *fd;
                while !self.buffer.is_empty() {
                    let data = self.buffer.readable();
                    let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
                    if n > 0 {
                        self.buffer.consume(n as usize);
                    } else if n < 0 {
                        if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                        {
                            continue;
                        }
                        return false;
                    } else {
                        return false;
                    }
                }
                true
            }
            FlushSink::Tls { session, socket } => {
                // Encrypt the whole buffer through the session onto the blocking
                // socket, then mark it consumed in one step.
                if !session.write_all_blocking(socket, self.buffer.readable()) {
                    return false;
                }
                let n = self.buffer.readable().len();
                self.buffer.consume(n);
                true
            }
        }
    }

    /// Builds a message with `build`; on a full buffer, drains to the flush
    /// sink (if streaming) and retries once.
    fn with_retry(
        &mut self,
        build: impl Fn(&mut FixedBuf) -> Result<(), WireFull>,
    ) -> Result<(), WireFull> {
        let mark = self.buffer.mark();
        match build(self.buffer) {
            Ok(()) => Ok(()),
            Err(WireFull) => {
                if matches!(self.flush, FlushSink::None) {
                    return Err(WireFull);
                }
                self.buffer.truncate_to(mark);
                if self.buffer.is_empty() {
                    // The message alone exceeds the whole buffer.
                    return Err(WireFull);
                }
                if !self.drain() {
                    return Err(WireFull);
                }
                build(self.buffer)
            }
        }
    }

    pub fn parse_complete(&mut self) -> Result<(), WireFull> {
        MsgOut::begin(self.buffer, wire::MSG_PARSE_COMPLETE).finish()
    }

    pub fn bind_complete(&mut self) -> Result<(), WireFull> {
        MsgOut::begin(self.buffer, wire::MSG_BIND_COMPLETE).finish()
    }

    pub fn close_complete(&mut self) -> Result<(), WireFull> {
        MsgOut::begin(self.buffer, wire::MSG_CLOSE_COMPLETE).finish()
    }

    pub fn no_data(&mut self) -> Result<(), WireFull> {
        MsgOut::begin(self.buffer, wire::MSG_NO_DATA).finish()
    }

    /// All parameters are described as text for now.
    pub fn parameter_description(&mut self, oids: &[i32]) -> Result<(), WireFull> {
        let mut m = MsgOut::begin(self.buffer, wire::MSG_PARAMETER_DESCRIPTION);
        m.i16(oids.len() as i16);
        for &oid in oids {
            m.i32(oid);
        }
        m.finish()
    }

    pub fn auth_ok(&mut self) -> Result<(), WireFull> {
        let mut m = MsgOut::begin(self.buffer, wire::MSG_AUTHENTICATION);
        m.i32(wire::AUTH_OK);
        m.finish()
    }

    pub fn auth_cleartext_password(&mut self) -> Result<(), WireFull> {
        let mut m = MsgOut::begin(self.buffer, wire::MSG_AUTHENTICATION);
        m.i32(wire::AUTH_CLEARTEXT);
        m.finish()
    }

    pub fn auth_sasl_mechanisms(&mut self) -> Result<(), WireFull> {
        let mut m = MsgOut::begin(self.buffer, wire::MSG_AUTHENTICATION);
        m.i32(wire::AUTH_SASL);
        m.cstr("SCRAM-SHA-256");
        m.u8(0); // end of mechanism list
        m.finish()
    }

    pub fn auth_sasl_continue(&mut self, payload: &str) -> Result<(), WireFull> {
        let mut m = MsgOut::begin(self.buffer, wire::MSG_AUTHENTICATION);
        m.i32(wire::AUTH_SASL_CONTINUE);
        m.bytes(payload.as_bytes());
        m.finish()
    }

    pub fn auth_sasl_final(&mut self, payload: &str) -> Result<(), WireFull> {
        let mut m = MsgOut::begin(self.buffer, wire::MSG_AUTHENTICATION);
        m.i32(wire::AUTH_SASL_FINAL);
        m.bytes(payload.as_bytes());
        m.finish()
    }

    pub fn parameter_status(&mut self, name: &str, value: &str) -> Result<(), WireFull> {
        let mut m = MsgOut::begin(self.buffer, wire::MSG_PARAMETER_STATUS);
        m.cstr(name).cstr(value);
        m.finish()
    }

    pub fn backend_key_data(&mut self, pid: i32, key: &[u8]) -> Result<(), WireFull> {
        let mut m = MsgOut::begin(self.buffer, wire::MSG_BACKEND_KEY_DATA);
        m.i32(pid).bytes(key);
        m.finish()
    }

    pub fn negotiate_protocol_version(
        &mut self,
        newest_minor: i32,
        unrecognized_options: &[&str],
    ) -> Result<(), WireFull> {
        let mut m = MsgOut::begin(self.buffer, wire::MSG_NEGOTIATE_VERSION);
        m.i32(newest_minor);
        m.i32(unrecognized_options.len() as i32);
        for opt in unrecognized_options {
            m.cstr(opt);
        }
        m.finish()
    }

    pub fn ready_for_query(&mut self, tx_status: u8) -> Result<(), WireFull> {
        let mut m = MsgOut::begin(self.buffer, wire::MSG_READY_FOR_QUERY);
        m.u8(tx_status);
        m.finish()
    }

    pub fn row_description(&mut self, columns: &[ColDesc]) -> Result<(), WireFull> {
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
        if let Some(buffer) = self.alternate_buffer.as_deref_mut() {
            let formats = self.alternate_formats;
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
            m.finish()?;
        }
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

    pub(crate) fn data_row_prepared_alternate(
        &mut self,
        values: &[Datum],
        write: &dyn Fn(&mut MsgOut),
    ) -> Result<(), WireFull> {
        let Some(buffer) = self.alternate_buffer.as_deref_mut() else {
            return Ok(());
        };
        let mut message = MsgOut::begin(buffer, wire::MSG_DATA_ROW);
        message.i16(values.len() as i16);
        write(&mut message);
        message.finish()
    }

    pub(crate) fn result_formats(&self) -> ResultFmt {
        self.formats
    }

    pub(crate) fn alternate_result_formats(&self) -> Option<ResultFmt> {
        self.alternate_buffer
            .as_ref()
            .map(|_| self.alternate_formats)
    }

    /// Rewrites a captured SQL-cursor RowDescription for this FETCH's result
    /// formats. Cursor sealing has already validated the message boundary.
    pub(crate) fn cursor_row_description(
        &mut self,
        captured: &[u8],
        formats: ResultFmt,
    ) -> Result<(), WireFull> {
        if self.suppress_row_description || self.discard_query_output {
            return Ok(());
        }
        let payload = &captured[5..];
        let count = i16::from_be_bytes(payload[..2].try_into().unwrap()) as usize;
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

    /// Builds one FETCH DataRow from the text and binary representations
    /// captured during DECLARE, including mixed per-column formats.
    pub(crate) fn cursor_data_row(
        &mut self,
        text: &[u8],
        binary: &[u8],
        formats: ResultFmt,
    ) -> Result<(), WireFull> {
        fn payload(message: &[u8]) -> (&[u8], usize) {
            let payload = &message[5..];
            let count = i16::from_be_bytes(payload[..2].try_into().unwrap()) as usize;
            (payload, count)
        }
        let (text, count) = payload(text);
        let (binary, binary_count) = payload(binary);
        debug_assert_eq!(count, binary_count);
        self.with_retry(|buffer| {
            let mut output = MsgOut::begin(buffer, wire::MSG_DATA_ROW);
            output.i16(count as i16);
            let mut text_at = 2usize;
            let mut binary_at = 2usize;
            for column in 0..count {
                let text_len = i32::from_be_bytes(text[text_at..text_at + 4].try_into().unwrap());
                let binary_len =
                    i32::from_be_bytes(binary[binary_at..binary_at + 4].try_into().unwrap());
                let (source, at, len) = if formats.is_binary(column) {
                    (binary, binary_at, binary_len)
                } else {
                    (text, text_at, text_len)
                };
                output.i32(len);
                if len >= 0 {
                    output.bytes(&source[at + 4..at + 4 + len as usize]);
                }
                text_at += 4 + text_len.max(0) as usize;
                binary_at += 4 + binary_len.max(0) as usize;
            }
            output.finish()
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
        m: &mut MsgOut,
        v: &Datum,
        render: crate::sql::guc::RenderContext,
    ) {
        {
            match v {
                Datum::Null => {
                    m.i32(-1);
                }
                Datum::Text(s)
                | Datum::Bpchar(s)
                | Datum::Regtype { name: s, .. }
                | Datum::RegObject { name: s, .. } => {
                    m.i32(s.len() as i32);
                    m.bytes(s.as_bytes());
                }
                Datum::Bytea(b) => {
                    if render.bytea_escape {
                        // bytea_output = escape: printable ASCII verbatim,
                        // backslash doubled, everything else \nnn octal.
                        let escaped_len: usize = b
                            .iter()
                            .map(|&byte| match byte {
                                b'\\' => 2,
                                0x20..=0x7e => 1,
                                _ => 4,
                            })
                            .sum();
                        m.i32(escaped_len as i32);
                        for &byte in *b {
                            match byte {
                                b'\\' => {
                                    m.bytes(b"\\\\");
                                }
                                0x20..=0x7e => {
                                    m.bytes(&[byte]);
                                }
                                _ => {
                                    m.bytes(&[
                                        b'\\',
                                        b'0' + (byte >> 6),
                                        b'0' + ((byte >> 3) & 7),
                                        b'0' + (byte & 7),
                                    ]);
                                }
                            }
                        }
                    } else {
                        // \x hex, streamed straight into the send buffer.
                        m.i32((2 + b.len() * 2) as i32);
                        m.bytes(b"\\x");
                        const HEX: &[u8; 16] = b"0123456789abcdef";
                        for byte in *b {
                            m.bytes(&[HEX[(byte >> 4) as usize], HEX[(byte & 0xf) as usize]]);
                        }
                    }
                }
                Datum::Numeric(nm) => {
                    // Numeric text can be long (up to MAX_NDIGITS*4 digits);
                    // render into a bounded stack buffer.
                    let text = stack_format!(4200, "{}", nm);
                    debug_assert!(!text.is_truncated());
                    m.i32(text.as_str().len() as i32);
                    m.bytes(text.as_str().as_bytes());
                }
                // Date/time output honors the session DateStyle and time zone.
                Datum::Date(d) => {
                    let text = crate::sql::datetime::format_date_styled(*d, render.datestyle);
                    m.i32(text.as_str().len() as i32);
                    m.bytes(text.as_str().as_bytes());
                }
                Datum::Timestamp(t) | Datum::Timestamptz(t) => {
                    let with_timezone = matches!(v, Datum::Timestamptz(_));
                    let text = crate::sql::datetime::format_timestamp_styled(
                        *t,
                        with_timezone,
                        render.datestyle,
                        render.parsed_timezone,
                    );
                    m.i32(text.as_str().len() as i32);
                    m.bytes(text.as_str().as_bytes());
                }
                Datum::Interval(interval) => {
                    let text = crate::sql::datetime::format_interval_styled(
                        *interval,
                        render.intervalstyle,
                    );
                    m.i32(text.as_str().len() as i32);
                    m.bytes(text.as_str().as_bytes());
                }
                // Records, JSON, ranges, multiranges, bit strings — anything
                // whose text can be arbitrarily wide: count the length, emit it,
                // then stream Display straight to the send buffer (no fixed-size
                // scratch that would silently truncate a long value).
                other => {
                    use core::fmt::Write as _;
                    struct Counter(usize);
                    impl core::fmt::Write for Counter {
                        fn write_str(&mut self, s: &str) -> core::fmt::Result {
                            self.0 += s.len();
                            Ok(())
                        }
                    }
                    struct MsgWriter<'w, 'b>(&'w mut MsgOut<'b>);
                    impl core::fmt::Write for MsgWriter<'_, '_> {
                        fn write_str(&mut self, s: &str) -> core::fmt::Result {
                            self.0.bytes(s.as_bytes());
                            Ok(())
                        }
                    }
                    let mut counter = Counter(0);
                    let _ = write!(counter, "{other}");
                    m.i32(counter.0 as i32);
                    let _ = write!(MsgWriter(m), "{other}");
                }
            }
        }
    }

    /// One value's wire-text form as an arena string — the same output
    /// function semantics `encode_value_text` streams onto the wire (styled
    /// dates and timestamps, GUC-honoring bytea, Display for the rest), for
    /// consumers that must post-process the text (COPY escapes it).
    /// `None` is NULL.
    pub fn datum_wire_text<'a>(
        v: &Datum<'a>,
        render: crate::sql::guc::RenderContext,
        arena: &'a crate::mem::arena::Arena,
    ) -> Result<Option<&'a str>, crate::sql::eval::SqlError> {
        use crate::sql::eval::sqlstate;
        let full = || {
            crate::sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "COPY row exceeds the statement arena"
            )
        };
        Ok(Some(match v {
            Datum::Null => return Ok(None),
            Datum::Text(s) | Datum::Bpchar(s) => s,
            Datum::Bytea(b) if render.bytea_escape => {
                let escaped_len: usize = b
                    .iter()
                    .map(|&byte| match byte {
                        b'\\' => 2,
                        0x20..=0x7e => 1,
                        _ => 4,
                    })
                    .sum();
                let out = arena
                    .alloc_slice_with(escaped_len, |_| 0u8)
                    .map_err(|_| full())?;
                let mut at = 0;
                for &byte in b.iter() {
                    match byte {
                        b'\\' => {
                            out[at..at + 2].copy_from_slice(b"\\\\");
                            at += 2;
                        }
                        0x20..=0x7e => {
                            out[at] = byte;
                            at += 1;
                        }
                        _ => {
                            out[at] = b'\\';
                            out[at + 1] = b'0' + (byte >> 6);
                            out[at + 2] = b'0' + ((byte >> 3) & 7);
                            out[at + 3] = b'0' + (byte & 7);
                            at += 4;
                        }
                    }
                }
                core::str::from_utf8(out).expect("escaped bytea is ASCII")
            }
            Datum::Date(d) => arena
                .alloc_str(crate::sql::datetime::format_date_styled(*d, render.datestyle).as_str())
                .map_err(|_| full())?,
            Datum::Timestamp(t) | Datum::Timestamptz(t) => {
                let with_timezone = matches!(v, Datum::Timestamptz(_));
                let text = crate::sql::datetime::format_timestamp_styled(
                    *t,
                    with_timezone,
                    render.datestyle,
                    render.parsed_timezone,
                );
                arena.alloc_str(text.as_str()).map_err(|_| full())?
            }
            Datum::Interval(interval) => arena
                .alloc_str(
                    crate::sql::datetime::format_interval_styled(*interval, render.intervalstyle)
                        .as_str(),
                )
                .map_err(|_| full())?,
            other => arena.alloc_str_display(other).map_err(|_| full())?,
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
                Datum::Text(s) | Datum::Bpchar(s) => {
                    m.i32(s.len() as i32);
                    m.bytes(s.as_bytes());
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
                    let elem_oid = match element {
                        crate::sql::types::ArrElem::Domain { slot, .. } => {
                            crate::sql::types::oid::domain_oid(*slot)
                        }
                        _ => element.to_coltype().oid(),
                    };
                    let shape = crate::sql::array::shape(raw).expect("array datum invariant");
                    let count = shape.element_count();
                    m.field(|m| {
                        m.i32(shape.dimension_count() as i32);
                        let has_null = (0..count).any(|i| {
                            crate::sql::array::get(raw, *element, i).is_none_or(|d| d.is_null())
                        });
                        m.i32(i32::from(has_null));
                        m.i32(elem_oid);
                        for index in 0..shape.dimension_count() {
                            m.i32(shape.dimension(index).unwrap() as i32);
                            m.i32(shape.lower_bound(index).unwrap());
                        }
                        for i in 0..count {
                            let elem =
                                crate::sql::array::get(raw, *element, i).unwrap_or(Datum::Null);
                            Self::encode_value_binary(m, &elem);
                        }
                    });
                }
                Datum::Int2Vector(raw) => {
                    let count = raw.len() / 2;
                    m.field(|m| {
                        m.i32(if count == 0 { 0 } else { 1 });
                        m.i32(0);
                        m.i32(crate::sql::types::oid::INT2);
                        if count > 0 {
                            m.i32(count as i32);
                            m.i32(0);
                        }
                        for bytes in raw.as_chunks::<2>().0 {
                            m.i32(2);
                            m.bytes(&i16::from_le_bytes(*bytes).to_be_bytes());
                        }
                    });
                }
                Datum::OidVector(raw) => {
                    let count = raw.len() / 4;
                    m.field(|m| {
                        m.i32(if count == 0 { 0 } else { 1 });
                        m.i32(0);
                        m.i32(crate::sql::types::oid::OID);
                        if count > 0 {
                            m.i32(count as i32);
                            m.i32(0);
                        }
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
                        crate::sql::numeric::Sign::NaN => -0x4000, // 0xC000
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
            let mut components = [""; crate::sql::range::MAX_MULTIRANGE];
            let count = crate::sql::range::split_components(text, &mut components)
                .expect("multirange datums are canonical");
            message.i32(count as i32);
            for component in &components[..count] {
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

    /// Forwards pre-encoded wire message bytes verbatim (cursor FETCH replays
    /// captured RowDescription/DataRow messages).
    pub fn raw(&mut self, bytes: &[u8]) -> Result<(), WireFull> {
        if self.buffer.append(bytes) {
            Ok(())
        } else {
            Err(WireFull)
        }
    }

    pub fn command_complete(&mut self, tag: &str) -> Result<(), WireFull> {
        if self.discard_query_output || self.suppress_command_complete {
            return Ok(());
        }
        let mut m = MsgOut::begin(self.buffer, wire::MSG_COMMAND_COMPLETE);
        m.cstr(tag);
        m.finish()
    }

    /// Publishes an affected-row count alongside a DML command completion.
    /// Nested trigger execution consumes the count as typed execution state;
    /// clients still receive only PostgreSQL's ordinary command tag.
    pub(crate) fn command_complete_rows(&mut self, tag: &str, rows: u64) -> Result<(), WireFull> {
        self.set_affected_rows(rows);
        self.command_complete(tag)
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
        MsgOut::begin(self.buffer, wire::MSG_EMPTY_QUERY_RESPONSE).finish()
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
        let mut m = MsgOut::begin(self.buffer, wire::MSG_NOTICE_RESPONSE);
        m.u8(b'S');
        m.cstr(severity);
        m.u8(b'V');
        m.cstr(severity);
        m.u8(b'C');
        m.cstr(sqlstate);
        m.u8(b'M');
        m.cstr(message);
        if let Some(d) = diagnostic {
            m.u8(b'D');
            m.cstr(d.detail.as_str());
            if let Some(h) = &d.hint {
                m.u8(b'H');
                m.cstr(h.as_str());
            }
        }
        m.u8(0);
        m.finish()
    }

    /// ErrorResponse with the fields every client expects: severity (twice,
    /// localized and not), SQLSTATE, and message.
    pub fn error<S: AsRef<str>>(&mut self, sqlstate: S, message: &str) -> Result<(), WireFull> {
        let sqlstate = sqlstate.as_ref();
        let diagnostic = crate::sql::eval::take_diagnostic();
        let mut m = MsgOut::begin(self.buffer, wire::MSG_ERROR_RESPONSE);
        m.u8(b'S');
        m.cstr("ERROR");
        m.u8(b'V');
        m.cstr("ERROR");
        m.u8(b'C');
        m.cstr(sqlstate);
        m.u8(b'M');
        m.cstr(message);
        if let Some(d) = &diagnostic {
            m.u8(b'D');
            m.cstr(d.detail.as_str());
            if let Some(h) = &d.hint {
                m.u8(b'H');
                m.cstr(h.as_str());
            }
        }
        m.u8(0);
        m.finish()
    }

    /// Rolls back everything after `mark` and reports the error instead;
    /// used when a response overflows the send buffer.
    pub fn replace_with_overflow_error(&mut self, mark: usize) -> Result<(), WireFull> {
        self.buffer.truncate_to(mark);
        self.error(
            crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "statement response exceeds its configured buffer",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::Budget;
    use crate::mem::arena::Arena;
    use crate::sql::types::{RangeKind, RecordField, oid};

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
    fn copy_both_response_matches_postgresqls_zero_column_format() {
        let mut budget = Budget::new(1 << 16);
        let mut buffer = FixedBuf::new(&mut budget, "test", 256).unwrap();
        Responder::new(&mut buffer).copy_both_response().unwrap();
        assert_eq!(buffer.readable(), &[b'W', 0, 0, 0, 7, 0, 0, 0]);
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
}
