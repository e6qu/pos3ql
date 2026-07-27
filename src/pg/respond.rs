//! Backend message construction: the typed layer between the engine and
//! the raw send buffer.

use crate::mem::buffer::FixedBuf;
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
    pub const ALL_TEXT: Self = Self { codes: [false; MAX_RESULT_COLS], n: 0 };

    pub fn new(codes: [bool; MAX_RESULT_COLS], n: u16) -> Self {
        Self { codes, n }
    }

    /// Whether column `col` is requested in binary.
    fn is_binary(&self, col: usize) -> bool {
        match self.n {
            0 => false,
            1 => self.codes[0],
            _ => self.codes.get(col).copied().unwrap_or(false),
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
}

impl<'b> Responder<'b> {
    pub fn new(buffer: &'b mut FixedBuf) -> Self {
        Self {
            buffer,
            suppress_row_description: false,
            formats: ResultFmt::ALL_TEXT,
            flush: FlushSink::None,
            render: crate::sql::guc::RenderContext::default(),
        }
    }

    pub fn for_execute(buffer: &'b mut FixedBuf, formats: ResultFmt) -> Self {
        Self {
            buffer,
            suppress_row_description: true,
            formats,
            flush: FlushSink::None,
            render: crate::sql::guc::RenderContext::default(),
        }
    }

    /// Describe on a portal: RowDescription is emitted with the portal's
    /// result-format codes so the client decodes DataRows correctly.
    pub fn for_describe(buffer: &'b mut FixedBuf, formats: ResultFmt) -> Self {
        Self {
            buffer,
            suppress_row_description: false,
            formats,
            flush: FlushSink::None,
            render: crate::sql::guc::RenderContext::default(),
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
                        if std::io::Error::last_os_error().kind()
                            == std::io::ErrorKind::Interrupted
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
        if self.suppress_row_description {
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
        })
    }

    /// CopyInResponse / CopyOutResponse. `binary` selects PostgreSQL's format
    /// code (1 = binary, 0 = text/CSV) for the overall message and every column.
    pub fn copy_in_response(&mut self, n_columns: usize, binary: bool) -> Result<(), WireFull> {
        self.copy_response(b'G', n_columns, binary)
    }

    pub fn copy_out_response(&mut self, n_columns: usize, binary: bool) -> Result<(), WireFull> {
        self.copy_response(b'H', n_columns, binary)
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
        let formats = self.formats;
        let render = self.render_context();
        self.with_retry(|buffer| Self::build_data_row(buffer, values, formats, render))
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

    fn encode_value_text(
        m: &mut MsgOut,
        v: &Datum,
        render: crate::sql::guc::RenderContext,
    ) {
        {
            match v {
                Datum::Null => {
                    m.i32(-1);
                }
                Datum::Text(s) | Datum::Bpchar(s) => {
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
                let out = arena.alloc_slice_with(escaped_len, |_| 0u8).map_err(|_| full())?;
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
                Datum::Range { text, .. } | Datum::Multirange { text, .. } => {
                    // The binary range/multirange format requires re-parsing the
                    // bounds into typed datums (numeric bounds need an arena),
                    // which this arena-free wire primitive has no access to.
                    // COPY BINARY encodes ranges via the arena-aware path in
                    // `copy_out`; the only path reaching here is an extended-
                    // protocol Bind requesting binary results for a range column
                    // (rare), which receives the canonical text form.
                    m.i32(text.len() as i32);
                    m.bytes(text.as_bytes());
                }
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
                    let family = if net.family == 4 { 2u8 } else { 3u8 };
                    let is_cidr = u8::from(matches!(v, Datum::Cidr(_)));
                    m.i32(4 + i32::from(nb));
                    m.u8(family);
                    m.u8(net.bits);
                    m.u8(is_cidr);
                    m.u8(nb);
                    m.bytes(&net.addr[..nb as usize]);
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
                    // int32 ndim (0 for empty, else 1 — arrays are strictly
                    // one-dimensional here), int32 has-null flag, int32 element
                    // OID; then for a non-empty array one dim descriptor
                    // {count, lower-bound=1}; then each element as int32 length
                    // (-1 for NULL) + its binary (elements are always scalars).
                    let elem_oid = match element {
                        crate::sql::types::ArrElem::Domain { slot, .. } => {
                            crate::sql::types::oid::domain_oid(*slot)
                        }
                        _ => element.to_coltype().oid(),
                    };
                    let count = crate::sql::array::len(raw);
                    m.field(|m| {
                        m.i32(if count == 0 { 0 } else { 1 });
                        let has_null = (0..count).any(|i| {
                            crate::sql::array::get(raw, *element, i).is_none_or(|d| d.is_null())
                        });
                        m.i32(i32::from(has_null));
                        m.i32(elem_oid);
                        if count > 0 {
                            m.i32(count as i32);
                            m.i32(1);
                        }
                        for i in 0..count {
                            let elem = crate::sql::array::get(raw, *element, i)
                                .unwrap_or(Datum::Null);
                            Self::encode_value_binary(m, &elem);
                        }
                    });
                }
                Datum::Record(_) => {
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
                    let _ = write!(counter, "{v}");
                    m.i32(counter.0 as i32);
                    let _ = write!(MsgWriter(m), "{v}");
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
        let mut m = MsgOut::begin(self.buffer, wire::MSG_COMMAND_COMPLETE);
        m.cstr(tag);
        m.finish()
    }

    pub fn empty_query_response(&mut self) -> Result<(), WireFull> {
        MsgOut::begin(self.buffer, wire::MSG_EMPTY_QUERY_RESPONSE).finish()
    }

    /// NoticeResponse at NOTICE severity. Dropped when `client_min_messages`
    /// is above NOTICE (e.g. `warning`), matching PostgreSQL.
    pub fn notice(&mut self, sqlstate: &str, message: &str) -> Result<(), WireFull> {
        self.diagnostic(
            crate::sql::guc::MessageLevel::Notice,
            "NOTICE",
            sqlstate,
            message,
        )
    }

    /// NoticeResponse at WARNING severity. Dropped only when
    /// `client_min_messages` is above WARNING (i.e. `error`).
    pub fn warning(&mut self, sqlstate: &str, message: &str) -> Result<(), WireFull> {
        self.diagnostic(
            crate::sql::guc::MessageLevel::Warning,
            "WARNING",
            sqlstate,
            message,
        )
    }

    /// Emits a NoticeResponse (NOTICE or WARNING severity) unless the session's
    /// `client_min_messages` threshold filters it out. Same field layout as
    /// errors.
    fn diagnostic(
        &mut self,
        level: crate::sql::guc::MessageLevel,
        severity: &str,
        sqlstate: &str,
        message: &str,
    ) -> Result<(), WireFull> {
        // The stashed detail belongs to this diagnostic even when the level
        // filter drops it — take it either way so it cannot leak forward.
        let diagnostic = crate::sql::eval::take_diagnostic();
        if !self.render_context().min_message_level.allows(level) {
            return Ok(());
        }
        let mut m = MsgOut::begin(self.buffer, wire::MSG_NOTICE_RESPONSE);
        m.u8(b'S');
        m.cstr(severity);
        m.u8(b'V');
        m.cstr(severity);
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

    /// ErrorResponse with the fields every client expects: severity (twice,
    /// localized and not), SQLSTATE, and message.
    pub fn error(&mut self, sqlstate: &str, message: &str) -> Result<(), WireFull> {
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
            "response does not fit in the connection send buffer",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::Budget;
    use crate::sql::types::oid;

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
}
