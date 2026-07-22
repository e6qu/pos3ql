//! VSR protocol messages. Fixed-size, `Copy`, no allocation: the log
//! excerpt carried by view-change messages is bounded by [`MAX_LOG`].

use super::ReplicaId;

/// Maximum log entries a single DoViewChange/StartView can carry. Bounds
/// how far a lagging replica may be behind before it needs state transfer
/// (not modeled here); the simulator keeps within this.
pub(crate) const MAX_LOG: usize = 64;

/// One replicated operation: a client request identified for dedup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LogEntry {
    /// The view in which this operation was first prepared (used to pick the most
    /// up-to-date log during view change).
    pub view: u64,
    pub operation: u64,
    pub client: u32,
    pub request: u32,
    /// Opaque application payload.
    pub value: u64,
}

impl LogEntry {
    pub(crate) const EMPTY: LogEntry = LogEntry {
        view: 0,
        operation: 0,
        client: 0,
        request: 0,
        value: 0,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Message {
    pub from: ReplicaId,
    pub to: ReplicaId,
    pub body: MessageBody,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageBody {
    /// Primary → backups: replicate one operation.
    Prepare {
        view: u64,
        operation: u64,
        commit: u64,
        entry: LogEntry,
    },
    /// Backup → primary: operation accepted.
    PrepareOk { view: u64, operation: u64 },
    /// Primary → backups: heartbeat / commit advance (no new operation).
    Commit { view: u64, commit: u64 },
    /// Backup → all: begin a view change.
    StartViewChange { view: u64 },
    /// Replica → new primary: hand over log for the new view.
    DoViewChange {
        view: u64,
        /// The view in which the log was last normal (log-view `v'`).
        log_view: u64,
        operation: u64,
        commit: u64,
        log_len: u16,
        log: [LogEntry; MAX_LOG],
    },
    /// New primary → all: install the chosen log for the new view.
    StartView {
        view: u64,
        operation: u64,
        commit: u64,
        log_len: u16,
        log: [LogEntry; MAX_LOG],
    },
}
