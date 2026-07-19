//! Wire encoding for VSR messages: a fixed framing so replicas exchange
//! consensus traffic over TCP. Every message is `len:u32 | tag:u8 | fields`,
//! big-endian, with view-change logs carried as `count:u16` entries.
//!
//! Encoding is allocation-free into a caller buffer; decoding validates
//! lengths and rejects anything malformed rather than trusting the peer.

use super::message::{LogEntry, Message, MessageBody, MAX_LOG};
use super::ReplicaId;

const TAG_PREPARE: u8 = 1;
const TAG_PREPARE_OK: u8 = 2;
const TAG_COMMIT: u8 = 3;
const TAG_SVC: u8 = 4;
const TAG_DVC: u8 = 5;
const TAG_START_VIEW: u8 = 6;

const ENTRY_LEN: usize = 8 + 8 + 4 + 4 + 8; // view, operation, client, request, value

/// Largest possible encoded message (a DoViewChange/StartView with a full
/// log), used to size transport buffers.
pub const MAX_ENCODED: usize = 4 + 1 + 2 + 8 * 4 + 2 + MAX_LOG * ENTRY_LEN;

struct Writer<'a> {
    buffer: &'a mut [u8],
    at: usize,
}

impl<'a> Writer<'a> {
    fn new(buffer: &'a mut [u8]) -> Self {
        Self { buffer, at: 0 }
    }
    fn u8(&mut self, v: u8) {
        self.buffer[self.at] = v;
        self.at += 1;
    }
    fn u16(&mut self, v: u16) {
        self.buffer[self.at..self.at + 2].copy_from_slice(&v.to_be_bytes());
        self.at += 2;
    }
    fn u32(&mut self, v: u32) {
        self.buffer[self.at..self.at + 4].copy_from_slice(&v.to_be_bytes());
        self.at += 4;
    }
    fn u64(&mut self, v: u64) {
        self.buffer[self.at..self.at + 8].copy_from_slice(&v.to_be_bytes());
        self.at += 8;
    }
    fn entry(&mut self, e: &LogEntry) {
        self.u64(e.view);
        self.u64(e.operation);
        self.u32(e.client);
        self.u32(e.request);
        self.u64(e.value);
    }
}

/// Encodes `msg` into `buffer`, returning the number of bytes written, or
/// `None` if the buffer is too small. `from`/`to` are carried in the frame.
pub fn encode(msg: &Message, buffer: &mut [u8]) -> Option<usize> {
    if buffer.len() < MAX_ENCODED {
        // Encoders always get a MAX_ENCODED-sized buffer; refuse otherwise
        // rather than risk a partial frame.
        return None;
    }
    // Reserve 4 bytes for the length prefix; body starts at 4.
    let mut w = Writer::new(buffer);
    w.at = 4;
    w.u8(msg.from);
    w.u8(msg.to);
    match &msg.body {
        MessageBody::Prepare { view, operation, commit, entry } => {
            w.u8(TAG_PREPARE);
            w.u64(*view);
            w.u64(*operation);
            w.u64(*commit);
            w.entry(entry);
        }
        MessageBody::PrepareOk { view, operation } => {
            w.u8(TAG_PREPARE_OK);
            w.u64(*view);
            w.u64(*operation);
        }
        MessageBody::Commit { view, commit } => {
            w.u8(TAG_COMMIT);
            w.u64(*view);
            w.u64(*commit);
        }
        MessageBody::StartViewChange { view } => {
            w.u8(TAG_SVC);
            w.u64(*view);
        }
        MessageBody::DoViewChange {
            view,
            log_view,
            operation,
            commit,
            log_len,
            log,
        } => {
            w.u8(TAG_DVC);
            w.u64(*view);
            w.u64(*log_view);
            w.u64(*operation);
            w.u64(*commit);
            let n = (*log_len as usize).min(MAX_LOG);
            w.u16(n as u16);
            for e in &log[..n] {
                w.entry(e);
            }
        }
        MessageBody::StartView {
            view,
            operation,
            commit,
            log_len,
            log,
        } => {
            w.u8(TAG_START_VIEW);
            w.u64(*view);
            w.u64(*operation);
            w.u64(*commit);
            let n = (*log_len as usize).min(MAX_LOG);
            w.u16(n as u16);
            for e in &log[..n] {
                w.entry(e);
            }
        }
    }
    let total = w.at;
    let body_len = (total - 4) as u32;
    buffer[0..4].copy_from_slice(&body_len.to_be_bytes());
    Some(total)
}

struct Reader<'a> {
    buffer: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Option<u8> {
        let v = *self.buffer.get(self.at)?;
        self.at += 1;
        Some(v)
    }
    fn u16(&mut self) -> Option<u16> {
        let b = self.buffer.get(self.at..self.at + 2)?;
        self.at += 2;
        Some(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Option<u32> {
        let b = self.buffer.get(self.at..self.at + 4)?;
        self.at += 4;
        Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Option<u64> {
        let b = self.buffer.get(self.at..self.at + 8)?;
        self.at += 8;
        Some(u64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
    fn entry(&mut self) -> Option<LogEntry> {
        Some(LogEntry {
            view: self.u64()?,
            operation: self.u64()?,
            client: self.u32()?,
            request: self.u32()?,
            value: self.u64()?,
        })
    }
}

/// If `buffer` begins with a complete frame, decodes it and returns
/// `(message, bytes_consumed)`. Returns `Ok(None)` when more bytes are
/// needed, and `Err(())` on a malformed frame (the caller drops the peer).
#[allow(clippy::result_unit_err)]
pub fn decode(buffer: &[u8]) -> Result<Option<(Message, usize)>, ()> {
    if buffer.len() < 4 {
        return Ok(None);
    }
    let body_len = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
    if body_len == 0 || body_len > MAX_ENCODED {
        return Err(());
    }
    let total = 4 + body_len;
    if buffer.len() < total {
        return Ok(None);
    }
    let mut r = Reader {
        buffer: &buffer[..total],
        at: 4,
    };
    let mut parse = || -> Option<Message> {
        let from = r.u8()? as ReplicaId;
        let to = r.u8()? as ReplicaId;
        let tag = r.u8()?;
        let body = match tag {
            TAG_PREPARE => MessageBody::Prepare {
                view: r.u64()?,
                operation: r.u64()?,
                commit: r.u64()?,
                entry: r.entry()?,
            },
            TAG_PREPARE_OK => MessageBody::PrepareOk {
                view: r.u64()?,
                operation: r.u64()?,
            },
            TAG_COMMIT => MessageBody::Commit {
                view: r.u64()?,
                commit: r.u64()?,
            },
            TAG_SVC => MessageBody::StartViewChange { view: r.u64()? },
            TAG_DVC => {
                let view = r.u64()?;
                let log_view = r.u64()?;
                let operation = r.u64()?;
                let commit = r.u64()?;
                let n = r.u16()? as usize;
                if n > MAX_LOG {
                    return None;
                }
                let mut log = [LogEntry::EMPTY; MAX_LOG];
                for e in log.iter_mut().take(n) {
                    *e = r.entry()?;
                }
                MessageBody::DoViewChange {
                    view,
                    log_view,
                    operation,
                    commit,
                    log_len: n as u16,
                    log,
                }
            }
            TAG_START_VIEW => {
                let view = r.u64()?;
                let operation = r.u64()?;
                let commit = r.u64()?;
                let n = r.u16()? as usize;
                if n > MAX_LOG {
                    return None;
                }
                let mut log = [LogEntry::EMPTY; MAX_LOG];
                for e in log.iter_mut().take(n) {
                    *e = r.entry()?;
                }
                MessageBody::StartView {
                    view,
                    operation,
                    commit,
                    log_len: n as u16,
                    log,
                }
            }
            _ => return None,
        };
        Some(Message { from, to, body })
    };
    match parse() {
        Some(m) => Ok(Some((m, total))),
        None => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(m: Message) {
        let mut buffer = [0u8; MAX_ENCODED];
        let n = encode(&m, &mut buffer).expect("encode");
        let (decoded, consumed) = decode(&buffer[..n]).expect("decode ok").expect("complete");
        assert_eq!(consumed, n);
        assert_eq!(decoded, m);
    }

    #[test]
    fn every_message_kind_round_trips() {
        let entry = LogEntry {
            view: 3,
            operation: 9,
            client: 42,
            request: 7,
            value: 0xDEAD_BEEF,
        };
        roundtrip(Message {
            from: 1,
            to: 2,
            body: MessageBody::Prepare { view: 3, operation: 9, commit: 8, entry },
        });
        roundtrip(Message {
            from: 2,
            to: 0,
            body: MessageBody::PrepareOk { view: 3, operation: 9 },
        });
        roundtrip(Message {
            from: 0,
            to: 1,
            body: MessageBody::Commit { view: 3, commit: 9 },
        });
        roundtrip(Message {
            from: 1,
            to: 2,
            body: MessageBody::StartViewChange { view: 4 },
        });
        let mut log = [LogEntry::EMPTY; MAX_LOG];
        for (i, e) in log.iter_mut().enumerate().take(5) {
            *e = LogEntry {
                view: 1,
                operation: i as u64 + 1,
                client: 1,
                request: i as u32,
                value: i as u64 * 100,
            };
        }
        roundtrip(Message {
            from: 2,
            to: 1,
            body: MessageBody::DoViewChange {
                view: 4,
                log_view: 3,
                operation: 5,
                commit: 4,
                log_len: 5,
                log,
            },
        });
        roundtrip(Message {
            from: 1,
            to: 0,
            body: MessageBody::StartView {
                view: 4,
                operation: 5,
                commit: 5,
                log_len: 5,
                log,
            },
        });
    }

    #[test]
    fn partial_frame_needs_more() {
        let m = Message {
            from: 0,
            to: 1,
            body: MessageBody::Commit { view: 1, commit: 1 },
        };
        let mut buffer = [0u8; MAX_ENCODED];
        let n = encode(&m, &mut buffer).unwrap();
        // A prefix shorter than the full frame decodes to "need more".
        assert_eq!(decode(&buffer[..n - 1]), Ok(None));
        assert_eq!(decode(&buffer[..3]), Ok(None));
    }

    #[test]
    fn two_frames_decode_in_sequence() {
        let a = Message {
            from: 0,
            to: 1,
            body: MessageBody::Commit { view: 1, commit: 1 },
        };
        let b = Message {
            from: 0,
            to: 2,
            body: MessageBody::PrepareOk { view: 1, operation: 3 },
        };
        let mut buffer = [0u8; MAX_ENCODED * 2];
        let n1 = encode(&a, &mut buffer).unwrap();
        let mut second = [0u8; MAX_ENCODED];
        let n2 = encode(&b, &mut second).unwrap();
        buffer[n1..n1 + n2].copy_from_slice(&second[..n2]);

        let (m1, c1) = decode(&buffer[..n1 + n2]).unwrap().unwrap();
        assert_eq!(m1, a);
        let (m2, c2) = decode(&buffer[c1..n1 + n2]).unwrap().unwrap();
        assert_eq!(m2, b);
        assert_eq!(c1 + c2, n1 + n2);
    }

    #[test]
    fn garbage_tag_is_rejected() {
        let mut buffer = [0u8; MAX_ENCODED];
        buffer[0..4].copy_from_slice(&4u32.to_be_bytes());
        buffer[4] = 0; // from
        buffer[5] = 1; // to
        buffer[6] = 99; // unknown tag
        buffer[7] = 0;
        assert_eq!(decode(&buffer[..8]), Err(()));
    }
}
