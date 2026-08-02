//! LISTEN / NOTIFY: cross-connection asynchronous notifications.
//!
//! PostgreSQL's asynchronous-notification feature. A session registers interest
//! with `LISTEN channel`, another (or the same) session raises `NOTIFY channel
//! [, payload]`, and every listening session receives a `NotificationResponse`
//! carrying the notifying backend's PID, the channel, and the payload.
//!
//! The state lives on the [`Engine`](crate::sql::Engine) so a single owner
//! mediates every connection in the single-threaded event loop:
//!
//!   - a **registry** of `(connection id, channel)` pairs — who listens to what;
//!   - an **outbox** of committed notifications the server has not yet delivered.
//!
//! A transaction buffers the LISTEN/UNLISTEN it performs and the notifications
//! it raises; at COMMIT they are applied to the registry and moved to the
//! outbox, and at ROLLBACK they are discarded — matching PostgreSQL, where
//! `NOTIFY` and `LISTEN` take effect only on commit. Each buffered op carries
//! the connection id that owns it, so `commit_txn` can apply them with only the
//! transaction in hand. After a connection's message is processed the server
//! drains the outbox and fans each notification out to the listening slots.
//!
//! Every pool is fixed at startup; exhausting one is a loud error, never growth.

use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use crate::util::StackStr;

use core::fmt::Write as _;

/// A channel name. PostgreSQL channels are identifiers, capped at NAMEDATALEN-1
/// (63 bytes); 64 leaves room and matches the identifier limit elsewhere.
pub type Channel = StackStr<64>;

/// PostgreSQL rejects a NOTIFY payload of 8000 bytes or more, so the largest
/// accepted payload is 7999 bytes.
pub const MAX_PAYLOAD: usize = 7999;
pub type Payload = StackStr<MAX_PAYLOAD>;

/// Distinct channels one connection may listen on at once.
pub const CHANNELS_PER_CONN: usize = 32;
/// Notifications one transaction may buffer. Payloads live in a companion byte
/// buffer (see [`PER_TXN_PAYLOAD_BYTES`]), so these entries are compact.
pub const PER_TXN: usize = 64;
/// Total payload bytes one transaction's buffered notifications may hold.
pub const PER_TXN_PAYLOAD_BYTES: usize = 8192;
/// LISTEN/UNLISTEN operations one transaction may buffer.
pub const LISTEN_OPS_PER_TXN: usize = 32;
/// The engine-wide delivery outbox. A single dispatch can commit several
/// transactions before the server drains it, so it is sized above `PER_TXN`.
pub const OUTBOX: usize = 128;

/// Builds a channel name from text, matching PostgreSQL's identifier truncation
/// (a name longer than the limit is silently cut, not rejected).
pub fn channel(name: &str) -> Channel {
    let mut c = Channel::new();
    let _ = c.write_str(name);
    c
}

/// Builds a payload, enforcing PostgreSQL's length limit.
pub fn payload(text: &str) -> Result<Payload, SqlError> {
    let mut p = Payload::new();
    let _ = p.write_str(text);
    if p.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "payload string too long"
        ));
    }
    Ok(p)
}

/// One committed notification awaiting delivery. The payload is inline: the
/// delivery outbox is a single engine-wide pool, not one per connection, so the
/// full-size inline form is affordable there.
#[derive(Clone, Copy)]
pub struct Notification {
    pub pid: i32,
    pub channel: Channel,
    pub payload: Payload,
}

impl Notification {
    /// Builds a notification from a payload given as bytes (already validated
    /// against [`MAX_PAYLOAD`] when it was buffered).
    pub fn from_bytes(pid: i32, channel: Channel, payload_bytes: &[u8]) -> Self {
        let mut payload = Payload::new();
        // The bytes came from a validated payload, so they fit.
        let _ = core::str::from_utf8(payload_bytes).map(|text| payload.write_str(text));
        Self {
            pid,
            channel,
            payload,
        }
    }
}

/// A NOTIFY buffered on a transaction. The payload lives in the transaction's
/// companion byte buffer (so many small notifications, or one large one, share
/// a fixed budget), referenced by offset and length; only the small fields are
/// inline, keeping the per-transaction entry pool cheap.
#[derive(Clone, Copy)]
pub struct BufferedNotify {
    pub pid: i32,
    pub channel: Channel,
    pub payload_offset: usize,
    pub payload_len: usize,
}

/// A LISTEN/UNLISTEN buffered on a transaction. It carries the connection it
/// belongs to so the registry can be updated at commit without commit needing
/// the connection id threaded in.
#[derive(Clone, Copy)]
pub enum ListenOp {
    Listen { conn_id: i32, channel: Channel },
    Unlisten { conn_id: i32, channel: Channel },
    UnlistenAll { conn_id: i32 },
}

/// The engine-owned registry and delivery outbox.
pub struct NotifyState {
    /// `(connection id, channel)` — one entry per active registration.
    listeners: FixedVec<(i32, Channel)>,
    /// Committed notifications the server has not yet fanned out.
    outbox: FixedVec<Notification>,
}

impl NotifyState {
    pub fn new(
        budget: &mut Budget,
        max_listeners: usize,
        outbox_capacity: usize,
    ) -> Result<Self, BudgetError> {
        Ok(Self {
            listeners: FixedVec::new(budget, "notify_listeners", max_listeners)?,
            outbox: FixedVec::new(budget, "notify_outbox", outbox_capacity)?,
        })
    }

    /// True if `conn_id` is registered for `channel`.
    pub fn is_listening(&self, conn_id: i32, channel: &str) -> bool {
        self.listeners
            .as_slice()
            .iter()
            .any(|(id, ch)| *id == conn_id && ch.as_str() == channel)
    }

    fn listen(&mut self, conn_id: i32, ch: Channel) -> Result<(), SqlError> {
        // A duplicate LISTEN is a no-op, as in PostgreSQL.
        if self.is_listening(conn_id, ch.as_str()) {
            return Ok(());
        }
        self.listeners.push((conn_id, ch)).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many active LISTEN registrations (notify_listeners)"
            )
        })
    }

    fn unlisten(&mut self, conn_id: i32, ch: &str) {
        let mut i = 0;
        while i < self.listeners.len() {
            let (id, name) = self.listeners.as_slice()[i];
            if id == conn_id && name.as_str() == ch {
                self.listeners.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }

    fn unlisten_all(&mut self, conn_id: i32) {
        let mut i = 0;
        while i < self.listeners.len() {
            if self.listeners.as_slice()[i].0 == conn_id {
                self.listeners.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }

    /// Applies one buffered listen op at commit.
    pub fn apply(&mut self, op: ListenOp) -> Result<(), SqlError> {
        match op {
            ListenOp::Listen { conn_id, channel } => self.listen(conn_id, channel),
            ListenOp::Unlisten { conn_id, channel } => {
                self.unlisten(conn_id, channel.as_str());
                Ok(())
            }
            ListenOp::UnlistenAll { conn_id } => {
                self.unlisten_all(conn_id);
                Ok(())
            }
        }
    }

    /// Moves a committed notification into the delivery outbox.
    pub fn enqueue(&mut self, notification: Notification) -> Result<(), SqlError> {
        self.outbox.push(notification).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many pending notifications (notify_outbox)"
            )
        })
    }

    pub fn has_pending(&self) -> bool {
        !self.outbox.as_slice().is_empty()
    }

    pub fn outbox(&self) -> &[Notification] {
        self.outbox.as_slice()
    }

    pub fn clear_outbox(&mut self) {
        self.outbox.clear();
    }

    /// Drops every registration a closing connection held.
    pub fn drop_conn(&mut self, conn_id: i32) {
        self.unlisten_all(conn_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;

    fn state() -> NotifyState {
        let mut budget = Budget::new(1 << 20);
        NotifyState::new(&mut budget, 16, 16).unwrap()
    }

    #[test]
    fn registry_listen_unlisten_and_drop() {
        let mut s = state();
        assert!(
            s.apply(ListenOp::Listen {
                conn_id: 7,
                channel: channel("a")
            })
            .is_ok()
        );
        assert!(
            s.apply(ListenOp::Listen {
                conn_id: 7,
                channel: channel("b")
            })
            .is_ok()
        );
        assert!(
            s.apply(ListenOp::Listen {
                conn_id: 9,
                channel: channel("a")
            })
            .is_ok()
        );
        // A duplicate LISTEN is a no-op, not a second entry.
        assert!(
            s.apply(ListenOp::Listen {
                conn_id: 7,
                channel: channel("a")
            })
            .is_ok()
        );
        assert!(s.is_listening(7, "a"));
        assert!(s.is_listening(9, "a"));
        assert!(!s.is_listening(9, "b"));

        // UNLISTEN drops only the named channel for that connection.
        s.apply(ListenOp::Unlisten {
            conn_id: 7,
            channel: channel("a"),
        })
        .unwrap();
        assert!(!s.is_listening(7, "a"));
        assert!(s.is_listening(7, "b"));
        assert!(s.is_listening(9, "a")); // a different connection is untouched

        // Dropping a connection removes all of its registrations only.
        s.drop_conn(7);
        assert!(!s.is_listening(7, "b"));
        assert!(s.is_listening(9, "a"));

        // UNLISTEN * clears the rest for that connection.
        s.apply(ListenOp::UnlistenAll { conn_id: 9 }).unwrap();
        assert!(!s.is_listening(9, "a"));
    }

    #[test]
    fn outbox_enqueue_and_clear() {
        let mut s = state();
        assert!(!s.has_pending());
        s.enqueue(Notification {
            pid: 3,
            channel: channel("a"),
            payload: payload("hi").unwrap(),
        })
        .unwrap();
        assert!(s.has_pending());
        assert_eq!(s.outbox().len(), 1);
        assert_eq!(s.outbox()[0].pid, 3);
        assert_eq!(s.outbox()[0].channel.as_str(), "a");
        assert_eq!(s.outbox()[0].payload.as_str(), "hi");
        s.clear_outbox();
        assert!(!s.has_pending());
    }

    #[test]
    fn payload_length_limit() {
        assert!(payload(&"x".repeat(MAX_PAYLOAD)).is_ok());
        assert!(payload(&"x".repeat(MAX_PAYLOAD + 1)).is_err());
    }

    #[test]
    fn listener_pool_is_bounded() {
        let mut budget = Budget::new(1 << 20);
        let mut s = NotifyState::new(&mut budget, 2, 2).unwrap();
        assert!(
            s.apply(ListenOp::Listen {
                conn_id: 1,
                channel: channel("a")
            })
            .is_ok()
        );
        assert!(
            s.apply(ListenOp::Listen {
                conn_id: 1,
                channel: channel("b")
            })
            .is_ok()
        );
        // The third distinct registration exhausts the pool: a loud error.
        assert!(
            s.apply(ListenOp::Listen {
                conn_id: 1,
                channel: channel("c")
            })
            .is_err()
        );
    }
}
