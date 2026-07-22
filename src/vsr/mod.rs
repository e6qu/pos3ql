//! Viewstamped Replication (the protocol TigerBeetle uses), per "Viewstamped
//! Replication Revisited" (Liskov & Cowling, 2012).
//!
//! This is a sans-io deterministic state machine: it consumes messages and
//! logical ticks and emits messages, holding no clocks or sockets. The
//! server drives it over the reactor; the simulator ([`crate::sim`]) drives
//! N of them through a fault-injected virtual network, so consensus bugs
//! reproduce exactly from a seed.
//!
//! Scope of this implementation: normal operation (prepare / prepare_ok /
//! commit) and view change (start_view_change / do_view_change /
//! start_view), for clusters of 1..N with a majority quorum. Ops are
//! opaque `u64` payloads carrying a client request; the replicated log is
//! the abstraction the storage engine's WAL sits on in a full deployment.

#![expect(
    dead_code,
    reason = "built and unit-tested ahead of the phase that wires it into the \
              running server; `expect` rather than `allow` so this marker fails \
              once the code is actually reached"
)]

pub(crate) mod cluster;
pub(crate) mod codec;
pub(crate) mod message;
pub(crate) mod replica;

pub(crate) use message::Message;
pub(crate) use replica::{Replica, Status};

/// Replicas in a cluster, 0-indexed.
pub(crate) type ReplicaId = u8;

/// A cluster of `n` replicas tolerates `f` failures where `n = 2f + 1`.
/// The commit/view-change quorum is `f + 1`.
pub(crate) fn quorum(n: usize) -> usize {
    n / 2 + 1
}

/// The primary for a view is `view mod n` (VSR's round-robin rule).
pub(crate) fn primary_of(view: u64, n: usize) -> ReplicaId {
    (view % n as u64) as ReplicaId
}
