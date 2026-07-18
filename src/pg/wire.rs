//! PostgreSQL frontend/backend protocol v3 framing.
//!
//! Per <https://www.postgresql.org/docs/18/protocol-message-formats.html>:
//! backend messages are `type:u8 len:i32 payload`, where `len` includes
//! itself but not the type byte. The server speaks protocol 3.0 and 3.2
//! (3.1 was never assigned; 3.2 changes only the BackendKeyData cancel-key
//! length).

use crate::mem::buf::FixedBuf;

pub const PROTOCOL_3_0: i32 = 196608; // 3 << 16
pub const PROTOCOL_3_2: i32 = 196610; // 3 << 16 | 2
pub const NEWEST_MINOR: i32 = 2;

pub const REQUEST_SSL: i32 = 80877103;
pub const REQUEST_GSSENC: i32 = 80877104;
pub const REQUEST_CANCEL: i32 = 80877102;

// Backend message type bytes.
pub const MSG_AUTHENTICATION: u8 = b'R';
pub const AUTH_OK: i32 = 0;
pub const AUTH_CLEARTEXT: i32 = 3;
pub const AUTH_SASL: i32 = 10;
pub const AUTH_SASL_CONTINUE: i32 = 11;
pub const AUTH_SASL_FINAL: i32 = 12;
pub const MSG_PARAMETER_STATUS: u8 = b'S';
pub const MSG_BACKEND_KEY_DATA: u8 = b'K';
pub const MSG_READY_FOR_QUERY: u8 = b'Z';
pub const MSG_ROW_DESCRIPTION: u8 = b'T';
pub const MSG_DATA_ROW: u8 = b'D';
pub const MSG_COMMAND_COMPLETE: u8 = b'C';
pub const MSG_ERROR_RESPONSE: u8 = b'E';
pub const MSG_NOTICE_RESPONSE: u8 = b'N';
pub const MSG_EMPTY_QUERY_RESPONSE: u8 = b'I';
pub const MSG_NEGOTIATE_VERSION: u8 = b'v';
pub const MSG_PARSE_COMPLETE: u8 = b'1';
pub const MSG_BIND_COMPLETE: u8 = b'2';
pub const MSG_CLOSE_COMPLETE: u8 = b'3';
pub const MSG_NO_DATA: u8 = b'n';
pub const MSG_PARAMETER_DESCRIPTION: u8 = b't';
pub const MSG_PORTAL_SUSPENDED: u8 = b's';

// Frontend message type bytes.
pub const FMSG_QUERY: u8 = b'Q';
pub const FMSG_TERMINATE: u8 = b'X';
pub const FMSG_PARSE: u8 = b'P';
pub const FMSG_BIND: u8 = b'B';
pub const FMSG_EXECUTE: u8 = b'E';
pub const FMSG_DESCRIBE: u8 = b'D';
pub const FMSG_CLOSE: u8 = b'C';
pub const FMSG_SYNC: u8 = b'S';
pub const FMSG_FLUSH: u8 = b'H';
pub const FMSG_PASSWORD: u8 = b'p';

/// The send buffer cannot hold the message being built. The connection
/// must flush (or fail the statement) and retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireFull;

/// Writes one backend message into the send buffer, back-patching the
/// length on `finish`. Dropping without `finish` leaves garbage — callers
/// must either finish or `truncate_to` the pre-`begin` mark.
pub struct MsgOut<'a> {
    buf: &'a mut FixedBuf,
    len_at: usize,
    ok: bool,
}

impl<'a> MsgOut<'a> {
    pub fn begin(buf: &'a mut FixedBuf, msg_type: u8) -> Self {
        let ok = buf.append(&[msg_type]);
        let len_at = buf.mark();
        let ok = ok && buf.append(&[0, 0, 0, 0]);
        Self { buf, len_at, ok }
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.bytes(&[v])
    }

    pub fn i16(&mut self, v: i16) -> &mut Self {
        self.bytes(&v.to_be_bytes())
    }

    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.bytes(&v.to_be_bytes())
    }

    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.ok = self.ok && self.buf.append(v);
        self
    }

    /// NUL-terminated string. The text must not contain NUL itself.
    pub fn cstr(&mut self, v: &str) -> &mut Self {
        debug_assert!(!v.as_bytes().contains(&0));
        self.bytes(v.as_bytes());
        self.u8(0)
    }

    pub fn finish(self) -> Result<(), WireFull> {
        if !self.ok {
            return Err(WireFull);
        }
        let len = (self.buf.mark() - self.len_at) as i32;
        let filled = self.buf.filled_mut();
        filled[self.len_at..self.len_at + 4].copy_from_slice(&len.to_be_bytes());
        Ok(())
    }
}

/// Reads big-endian fields from a frontend message payload.
pub struct MsgIn<'a> {
    payload: &'a [u8],
    at: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed;

impl<'a> MsgIn<'a> {
    pub fn new(payload: &'a [u8]) -> Self {
        Self { payload, at: 0 }
    }

    pub fn i16(&mut self) -> Result<i16, Malformed> {
        let b = self.take(2)?;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }

    pub fn i32(&mut self) -> Result<i32, Malformed> {
        let b = self.take(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u8(&mut self) -> Result<u8, Malformed> {
        Ok(self.take(1)?[0])
    }

    /// NUL-terminated UTF-8 string.
    pub fn cstr(&mut self) -> Result<&'a str, Malformed> {
        let rest = &self.payload[self.at..];
        let nul = rest.iter().position(|&b| b == 0).ok_or(Malformed)?;
        let s = core::str::from_utf8(&rest[..nul]).map_err(|_| Malformed)?;
        self.at += nul + 1;
        Ok(s)
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8], Malformed> {
        if self.payload.len() - self.at < n {
            return Err(Malformed);
        }
        let s = &self.payload[self.at..self.at + n];
        self.at += n;
        Ok(s)
    }

    pub fn remaining(&self) -> usize {
        self.payload.len() - self.at
    }

    pub fn done(&self) -> bool {
        self.remaining() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::Budget;

    #[test]
    fn message_length_is_backpatched() {
        let mut budget = Budget::new(1024);
        let mut buf = FixedBuf::new(&mut budget, "test", 64).unwrap();
        let mut m = MsgOut::begin(&mut buf, MSG_READY_FOR_QUERY);
        m.u8(b'I');
        m.finish().unwrap();
        // Z, len=5 (4 length + 1 payload), 'I'
        assert_eq!(buf.readable(), &[b'Z', 0, 0, 0, 5, b'I']);
    }

    #[test]
    fn overflow_reports_wire_full_and_rolls_back() {
        let mut budget = Budget::new(1024);
        let mut buf = FixedBuf::new(&mut budget, "test", 8).unwrap();
        let mark = buf.mark();
        let mut m = MsgOut::begin(&mut buf, MSG_DATA_ROW);
        m.bytes(b"way too much data for this buffer");
        assert_eq!(m.finish(), Err(WireFull));
        buf.truncate_to(mark);
        assert!(buf.is_empty());
    }

    #[test]
    fn msg_in_reads_fields() {
        let payload = [0u8, 3, b'a', b'b', 0, 0, 0, 0, 42];
        let mut m = MsgIn::new(&payload);
        assert_eq!(m.i16().unwrap(), 3);
        assert_eq!(m.cstr().unwrap(), "ab");
        assert_eq!(m.i32().unwrap(), 42);
        assert!(m.done());
        assert_eq!(m.i16(), Err(Malformed));
    }
}
