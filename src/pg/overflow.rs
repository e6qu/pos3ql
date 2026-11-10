//! Per-connection result output that does not fit the send buffer.
//!
//! Logical output order is `send`, then the shared spool lease, then the
//! private reserve. Every tier holds only complete backend messages, so a
//! statement mark can roll back the unsent tail without splitting a frame and
//! the transport refills `send` at message boundaries.
//!
//! Rows (bulk messages) occupy the shared spool, leased from the server-wide
//! `result_spool_bytes` pool and returned as soon as it drains. The private
//! reserve has `conn_send_buffer_bytes` and is never shared, so the terminal
//! messages of a result (CommandComplete, ErrorResponse, ReadyForQuery) always
//! fit even when other connections hold the whole spool. Every message is
//! encoded in the private reserve first, so one message larger than
//! `conn_send_buffer_bytes` fails with an error naming that setting.

use std::sync::Arc;

use crate::mem::Budget;
use crate::mem::budget::BudgetError;
use crate::mem::buffer::FixedBuf;
use crate::mem::lease_pool::{LeaseError, LeasePool, MutableSlice};

use super::wire::WireFull;

/// What a message may occupy once `send` is full.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputClass {
    /// Result rows and other repeatable bulk output: send or shared spool only.
    Bulk,
    /// Protocol control and terminal messages: may also use the private reserve.
    Control,
}

/// Why a bulk message could not be queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OverflowExhaustion {
    /// The shared spool could not grow.
    Spool(LeaseError),
    /// No shared spool is attached to this output.
    NoSpool,
    /// One message is larger than the reserve it is encoded in.
    MessageTooLarge,
    /// Closing messages already occupy the reserve after the spool filled.
    ReserveInUse,
}

impl core::fmt::Display for OverflowExhaustion {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Spool(error) => write!(
                f,
                "statement response exceeds the client output spool: {error}"
            ),
            Self::NoSpool => f.write_str("statement response exceeds its configured buffer"),
            Self::MessageTooLarge => f.write_str("a result message exceeds conn_send_buffer_bytes"),
            Self::ReserveInUse => f.write_str(
                "statement response exceeds the client output spool \
                 (result_spool_bytes, result_spool_leases)",
            ),
        }
    }
}

pub struct ResultOverflow {
    private: FixedBuf,
    shared: Option<MutableSlice<u8>>,
    /// Bytes at the front of `shared` already moved to `send`.
    shared_start: usize,
    /// Total bytes the transport accepted since the connection opened. Marks
    /// use it to tell whether rolled-back output already left.
    written: u64,
    exhausted: Option<OverflowExhaustion>,
}

impl ResultOverflow {
    pub fn new(budget: &mut Budget, private_bytes: usize) -> Result<Self, BudgetError> {
        Ok(Self {
            private: FixedBuf::new(budget, "conn_output_reserve", private_bytes)?,
            shared: None,
            shared_start: 0,
            written: 0,
            exhausted: None,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.private.is_empty() && self.shared_len() == 0
    }

    pub fn len(&self) -> usize {
        self.private.len() + self.shared_len()
    }

    fn shared_len(&self) -> usize {
        self.shared
            .as_ref()
            .map_or(0, |shared| shared.len() - self.shared_start)
    }

    /// Bytes held in the shared spool lease.
    #[cfg(test)]
    pub(crate) fn shared_bytes(&self) -> usize {
        self.shared_len()
    }

    pub(crate) fn written(&self) -> u64 {
        self.written
    }

    pub(crate) fn record_written(&mut self, bytes: usize) {
        self.written = self.written.saturating_add(bytes as u64);
    }

    pub(crate) fn take_exhaustion(&mut self) -> Option<OverflowExhaustion> {
        self.exhausted.take()
    }

    /// Discards queued output and returns the spool lease.
    pub fn clear(&mut self) {
        self.private.clear();
        if let Some(mut shared) = self.shared.take() {
            shared.clear();
        }
        self.shared_start = 0;
        self.exhausted = None;
    }

    /// Resets the transport counter for a newly opened connection.
    pub fn reset(&mut self) {
        self.clear();
        self.written = 0;
    }

    /// Appends one complete message after all queued output.
    pub(crate) fn push(
        &mut self,
        spool: Option<&Arc<LeasePool>>,
        class: OutputClass,
        build: &dyn Fn(&mut FixedBuf) -> Result<(), WireFull>,
    ) -> Result<(), WireFull> {
        if !self.private.is_empty() {
            // The private reserve is the tail; ordering keeps later output there.
            if class == OutputClass::Bulk {
                return Err(self.exhaust(OverflowExhaustion::ReserveInUse));
            }
            return Self::build_into(&mut self.private, build);
        }
        if Self::build_into(&mut self.private, build).is_err() {
            return Err(self.exhaust(OverflowExhaustion::MessageTooLarge));
        }
        let Some(spool) = spool else {
            if class == OutputClass::Bulk {
                self.private.clear();
                return Err(self.exhaust(OverflowExhaustion::NoSpool));
            }
            return Ok(());
        };
        match self.spill_private(spool) {
            Ok(()) => Ok(()),
            Err(error) if class == OutputClass::Bulk => {
                self.private.clear();
                Err(self.exhaust(OverflowExhaustion::Spool(error)))
            }
            // A control message stays in the private reserve.
            Err(_) => Ok(()),
        }
    }

    fn exhaust(&mut self, reason: OverflowExhaustion) -> WireFull {
        self.exhausted = Some(reason);
        WireFull
    }

    fn build_into(
        buffer: &mut FixedBuf,
        build: &dyn Fn(&mut FixedBuf) -> Result<(), WireFull>,
    ) -> Result<(), WireFull> {
        let _ = buffer.writable();
        let mark = buffer.mark();
        build(buffer).inspect_err(|_| buffer.truncate_to(mark))
    }

    /// Moves the staged private message to the end of the shared spool.
    fn spill_private(&mut self, spool: &Arc<LeasePool>) -> Result<(), LeaseError> {
        let bytes = self.private.readable();
        let shared = match &mut self.shared {
            Some(shared) => shared,
            slot @ None => slot.insert(spool.lease::<u8>(0)?),
        };
        let needed = shared.len() + bytes.len();
        let grown = shared
            .capacity_entries()
            .saturating_mul(2)
            .min(shared.max_entries())
            .max(needed);
        if shared.reserve_exact(grown).is_err() {
            shared.reserve_exact(needed)?;
        }
        shared.extend_from_slice(bytes)?;
        self.private.clear();
        Ok(())
    }

    /// Moves whole messages into `send` while they fit. Returns whether any moved.
    pub(crate) fn refill(&mut self, send: &mut FixedBuf) -> bool {
        let mut moved = false;
        loop {
            if self.shared_len() != 0 {
                let shared = self.shared.as_mut().expect("nonempty spool");
                let pending = &shared.as_slice()[self.shared_start..];
                let length = message_length(pending);
                if length > send.writable().len() {
                    break;
                }
                assert!(send.append(&pending[..length]));
                self.shared_start += length;
                moved = true;
                if self.shared_start == shared.len() {
                    // Return the lease to other connections once it drains.
                    shared.clear();
                    self.shared = None;
                    self.shared_start = 0;
                } else if self.shared_start * 2 >= shared.len() {
                    let len = shared.len();
                    shared.as_mut_slice().copy_within(self.shared_start..len, 0);
                    shared.truncate(len - self.shared_start);
                    self.shared_start = 0;
                }
                continue;
            }
            if self.private.is_empty() {
                break;
            }
            let length = message_length(self.private.readable());
            if length > send.writable().len() {
                break;
            }
            assert!(send.append(&self.private.readable()[..length]));
            self.private.consume(length);
            moved = true;
        }
        moved
    }

    /// Removes up to `bytes` from the tail; returns how many were removed.
    pub(crate) fn truncate_tail(&mut self, mut bytes: usize) -> usize {
        let mut removed = 0;
        let private = bytes.min(self.private.len());
        self.private.truncate_to(self.private.mark() - private);
        removed += private;
        bytes -= private;
        if bytes != 0
            && let Some(shared) = &mut self.shared
        {
            let available = shared.len() - self.shared_start;
            let take = bytes.min(available);
            shared.truncate(shared.len() - take);
            removed += take;
            if shared.len() == self.shared_start {
                shared.clear();
                self.shared = None;
                self.shared_start = 0;
            }
        }
        removed
    }

    /// The last `bytes` bytes of queued output; they belong to one tier.
    pub(crate) fn tail(&self, bytes: usize) -> Option<&[u8]> {
        if !self.private.is_empty() {
            let readable = self.private.readable();
            return readable.get(readable.len().checked_sub(bytes)?..);
        }
        let shared = self.shared.as_ref()?;
        let readable = &shared.as_slice()[self.shared_start..];
        readable.get(readable.len().checked_sub(bytes)?..)
    }
}

/// Length of the complete backend message at the front of `bytes`.
fn message_length(bytes: &[u8]) -> usize {
    let length = i32::from_be_bytes(bytes[1..5].try_into().expect("message header"));
    1 + usize::try_from(length).expect("backend message length")
}

/// Removes the last `bytes` from the logical output (`send`, then overflow).
/// Returns false if fewer unsent bytes exist.
pub(crate) fn truncate_output(
    send: &mut FixedBuf,
    overflow: Option<&mut ResultOverflow>,
    bytes: usize,
) -> bool {
    let mut remaining = bytes;
    if let Some(overflow) = overflow {
        remaining -= overflow.truncate_tail(remaining);
    }
    if remaining > send.len() {
        return false;
    }
    send.truncate_to(send.mark() - remaining);
    true
}

/// A position in one connection's logical output stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputMark {
    queued: usize,
    written: u64,
}

impl OutputMark {
    pub(crate) fn new(queued: usize, written: u64) -> Self {
        Self { queued, written }
    }

    /// Rolls back a send buffer that had no overflow or transport writes
    /// since this mark was taken.
    #[cfg(test)]
    pub(crate) fn truncate_unsent(self, send: &mut FixedBuf) {
        let excess = self
            .excess(send.len(), self.written)
            .expect("mark precedes send");
        send.truncate_to(send.mark() - excess);
    }

    /// Whether this mark was taken no later than `other` within one output.
    pub(crate) fn precedes(self, other: Self) -> bool {
        self.queued + self.written as usize <= other.queued + other.written as usize
    }

    /// Unsent bytes to remove so the output ends at this mark, or `None`
    /// when bytes after the mark already reached the transport.
    pub(crate) fn excess(self, queued: usize, written: u64) -> Option<usize> {
        let sent_since = usize::try_from(written - self.written).ok()?;
        let kept = self.queued.checked_sub(sent_since)?;
        queued.checked_sub(kept)
    }
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::*;
    use crate::mem::lease_pool::Purpose;
    use crate::pg::wire::MsgOut;

    fn row(buffer: &mut FixedBuf, byte: u8, payload: usize) -> Result<(), WireFull> {
        let mut message = MsgOut::begin(buffer, b'D');
        for _ in 0..payload {
            message.u8(byte);
        }
        message.finish()
    }

    #[test]
    fn overflow_keeps_whole_messages_in_order_across_tiers() {
        let mut budget = Budget::new(1 << 20);
        let spool = LeasePool::new(&mut budget, Purpose::RESULT_SPOOL, 128, 1).unwrap();
        let mut overflow = ResultOverflow::new(&mut budget, 32).unwrap();
        let mut send = FixedBuf::new(&mut budget, "send", 32).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            for byte in 1..=4u8 {
                overflow
                    .push(Some(&spool), OutputClass::Bulk, &|buffer| {
                        row(buffer, byte, 27)
                    })
                    .unwrap();
            }
            assert_eq!(overflow.shared_bytes(), 128);
            // The spool cannot hold a fifth row; the reserve rejects rows.
            assert!(
                overflow
                    .push(Some(&spool), OutputClass::Bulk, &|buffer| row(
                        buffer, 5, 27
                    ))
                    .is_err()
            );
            assert!(matches!(
                overflow.take_exhaustion(),
                Some(OverflowExhaustion::Spool(_))
            ));
            overflow
                .push(Some(&spool), OutputClass::Control, &|buffer| {
                    let mut message = MsgOut::begin(buffer, b'Z');
                    message.u8(b'I');
                    message.finish()
                })
                .unwrap();
            assert_eq!(overflow.len(), 134);
            let mut seen = [0u8; 5];
            let mut count = 0;
            while !overflow.is_empty() {
                assert!(overflow.refill(&mut send));
                let readable = send.readable();
                seen[count] = if readable[0] == b'D' {
                    readable[5]
                } else {
                    b'Z'
                };
                count += 1;
                send.consume(send.len());
            }
            assert_eq!(&seen, &[1, 2, 3, 4, b'Z']);
            // A drained spool returns its lease.
            overflow
                .push(Some(&spool), OutputClass::Bulk, &|buffer| {
                    row(buffer, 6, 20)
                })
                .unwrap();
            assert_eq!(overflow.truncate_tail(25), 25);
            assert!(overflow.is_empty());
        });
    }

    #[test]
    fn output_marks_detect_bytes_that_already_left() {
        let mark = OutputMark::new(10, 0);
        assert_eq!(mark.excess(30, 0), Some(20));
        assert_eq!(mark.excess(25, 5), Some(20));
        assert_eq!(mark.excess(10, 15), None);
    }
}
