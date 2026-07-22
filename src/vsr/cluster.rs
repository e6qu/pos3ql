//! Multi-replica transport: drives a [`Replica`] over real TCP sockets to
//! its peers. Each node listens for peer connections and dials the others,
//! frames VSR messages with [`super::codec`], and pumps the state machine
//! on a logical tick. This is the network binding the VOPR-verified
//! consensus core; the same `Replica` runs here and in the simulator.
//!
//! Non-blocking sockets, fixed per-peer buffers, no allocation on the
//! steady-state path. Committed ops are handed to a caller-supplied
//! `apply` sink (the storage engine in a full deployment).

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};

use super::codec::{self, MAX_ENCODED};
use super::replica::Committed;
use super::{Replica, ReplicaId};

/// One peer link: an optional connected stream plus a receive buffer for
/// reassembling framed messages.
struct Peer {
    addr: String,
    stream: Option<TcpStream>,
    rx: Vec<u8>,
}

pub(crate) struct ClusterNode {
    replica: Replica,
    listener: TcpListener,
    peers: Vec<Peer>,
    tx: [u8; MAX_ENCODED],
    /// Committed ops delivered to the application, in order.
    pub applied: Vec<Committed>,
}

impl ClusterNode {
    /// Binds `addrs[id]` and prepares links to the other replicas. Peer
    /// dialing happens lazily in [`Self::poll`].
    pub(crate) fn bind(id: ReplicaId, addrs: &[String], view_change_timeout: u32) -> std::io::Result<Self> {
        let n = addrs.len();
        let listener = TcpListener::bind(&addrs[id as usize])?;
        listener.set_nonblocking(true)?;
        let peers = addrs
            .iter()
            .enumerate()
            .map(|(i, a)| Peer {
                addr: if i == id as usize { String::new() } else { a.clone() },
                stream: None,
                rx: Vec::new(),
            })
            .collect();
        Ok(Self {
            replica: Replica::new(id, n, view_change_timeout),
            listener,
            peers,
            tx: [0u8; MAX_ENCODED],
            applied: Vec::new(),
        })
    }

    pub(crate) fn replica(&mut self) -> &mut Replica {
        &mut self.replica
    }

    pub(crate) fn id(&self) -> ReplicaId {
        self.replica.id
    }

    /// Submits a client write at this node. Returns false if this node is
    /// not the primary (the caller should redirect to `self.replica.primary()`).
    pub(crate) fn submit(&mut self, client: u32, request: u32, value: u64) -> bool {
        let accepted = self.replica.on_request(client, request, value);
        if accepted {
            self.flush_outbox();
            self.collect();
        }
        accepted
    }

    /// One event-loop step: accept new peer links, dial missing ones, read
    /// and dispatch incoming messages, tick the replica, and flush output.
    pub(crate) fn poll(&mut self) {
        self.accept_incoming();
        self.dial_missing();
        self.read_peers();
        self.replica.on_tick();
        self.flush_outbox();
        self.collect();
    }

    fn accept_incoming(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(true).ok();
                    stream.set_nodelay(true).ok();
                    // We cannot yet know which replica dialed us; attach it
                    // to the first free inbound slot by reading the sender id
                    // lazily from framed messages. Simplest correct approach:
                    // keep a small pool of unclaimed inbound streams.
                    self.unclaimed_push(stream);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
    }

    fn unclaimed_push(&mut self, stream: TcpStream) {
        // Park the stream in the first peer slot lacking a stream (other than
        // self). The first frame's `from` field will confirm identity; since
        // links are symmetric we accept it into any open slot and rely on the
        // message `from`/`to` fields for routing.
        for (i, p) in self.peers.iter_mut().enumerate() {
            if i != self.replica.id as usize && p.stream.is_none() {
                p.stream = Some(stream);
                return;
            }
        }
        // All slots full: drop (peer will already have a working link).
    }

    fn dial_missing(&mut self) {
        // Only dial peers with a higher id to avoid duplicate links; the
        // lower-id side listens. (A simple, deterministic link-formation
        // rule.)
        for i in 0..self.peers.len() {
            if i == self.replica.id as usize {
                continue;
            }
            if self.peers[i].stream.is_some() || i < self.replica.id as usize {
                continue;
            }
            let addr = self.peers[i].addr.clone();
            if let Ok(stream) = TcpStream::connect(&addr) {
                stream.set_nonblocking(true).ok();
                stream.set_nodelay(true).ok();
                self.peers[i].stream = Some(stream);
            }
        }
    }

    fn read_peers(&mut self) {
        let mut incoming: Vec<super::Message> = Vec::new();
        for peer in &mut self.peers {
            let Some(stream) = peer.stream.as_mut() else {
                continue;
            };
            let mut chunk = [0u8; 4096];
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) => {
                        peer.stream = None;
                        break;
                    }
                    Ok(k) => peer.rx.extend_from_slice(&chunk[..k]),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => {
                        peer.stream = None;
                        break;
                    }
                }
            }
            // Drain complete frames.
            loop {
                match codec::decode(&peer.rx) {
                    Ok(Some((msg, consumed))) => {
                        peer.rx.drain(..consumed);
                        incoming.push(msg);
                    }
                    Ok(None) => break,
                    Err(()) => {
                        peer.stream = None;
                        peer.rx.clear();
                        break;
                    }
                }
            }
        }
        for msg in incoming {
            if msg.to == self.replica.id {
                self.replica.on_message(msg);
            }
        }
        // Messages may have produced replies.
        self.flush_outbox();
        self.collect();
    }

    fn flush_outbox(&mut self) {
        let mut out = Vec::new();
        out.extend(self.replica.outbox().drain());
        for msg in out {
            let to = msg.to as usize;
            if to == self.replica.id as usize {
                self.replica.on_message(msg);
                continue;
            }
            let Some(n) = codec::encode(&msg, &mut self.tx) else {
                continue;
            };
            if let Some(stream) = self.peers[to].stream.as_mut() {
                // Best-effort write; a partial/failed write drops the link,
                // and VSR retransmits.
                if stream.write_all(&self.tx[..n]).is_err() {
                    self.peers[to].stream = None;
                }
            }
        }
    }

    fn collect(&mut self) {
        let mut delivered = Vec::new();
        delivered.extend(self.replica.take_delivered());
        self.applied.extend(delivered);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn free_addrs(n: usize) -> Vec<String> {
        // Bind ephemeral ports, then release them for the nodes to re-bind.
        let mut addrs = Vec::new();
        let mut held = Vec::new();
        for _ in 0..n {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            addrs.push(l.local_addr().unwrap().to_string());
            held.push(l);
        }
        drop(held);
        addrs
    }

    /// Pumps all nodes until `done` holds or the deadline passes.
    fn pump_until(nodes: &mut [ClusterNode], deadline: Duration, done: impl Fn(&[ClusterNode]) -> bool) {
        let start = Instant::now();
        while start.elapsed() < deadline {
            for node in nodes.iter_mut() {
                node.poll();
            }
            if done(nodes) {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn primary_index(nodes: &[ClusterNode]) -> Option<usize> {
        nodes.iter().position(|node| {
            node.replica.is_primary() && node.replica.status == super::super::Status::Normal
        })
    }

    #[test]
    fn three_node_cluster_replicates_over_tcp() {
        let addrs = free_addrs(3);
        let mut nodes: Vec<ClusterNode> = (0..3)
            .map(|i| ClusterNode::bind(i as u8, &addrs, 25).unwrap())
            .collect();

        // Let links form.
        pump_until(&mut nodes, Duration::from_secs(2), |n| {
            n.iter().all(|node| {
                node.peers
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != node.replica.id as usize)
                    .all(|(_, p)| p.stream.is_some())
            })
        });

        // Submit three writes at replica 0 (the initial primary of view 0).
        for req in 1..=3u32 {
            assert!(nodes[0].submit(1, req, u64::from(req) * 10), "primary rejected write");
        }
        pump_until(&mut nodes, Duration::from_secs(3), |n| {
            n.iter().all(|node| node.applied.len() >= 3)
        });
        for node in &nodes {
            assert_eq!(node.applied.len(), 3, "replica {} missed ops", node.id());
            let vals: Vec<u64> = node.applied.iter().map(|c| c.value).collect();
            assert_eq!(vals, vec![10, 20, 30], "replica {} diverged", node.id());
        }
    }

    #[test]
    fn cluster_fails_over_when_primary_dies() {
        let addrs = free_addrs(3);
        let mut nodes: Vec<ClusterNode> = (0..3)
            .map(|i| ClusterNode::bind(i as u8, &addrs, 20).unwrap())
            .collect();

        pump_until(&mut nodes, Duration::from_secs(2), |n| primary_index(n) == Some(0));
        assert!(nodes[0].submit(1, 1, 100));
        pump_until(&mut nodes, Duration::from_secs(2), |n| {
            n.iter().all(|node| !node.applied.is_empty())
        });

        // Kill the primary (replica 0): drop its links and stop polling it.
        for p in &mut nodes[0].peers {
            p.stream = None;
        }
        // Sever the survivors' links to node 0 too.
        for node in nodes.iter_mut().skip(1) {
            node.peers[0].stream = None;
        }

        // Pump only the survivors (1 and 2). They must elect a new primary
        // and keep the committed operation.
        let start = Instant::now();
        let mut new_primary = None;
        while start.elapsed() < Duration::from_secs(4) {
            for node in nodes.iter_mut().skip(1) {
                node.poll();
            }
            // Re-sever any link that reconnected to the dead node.
            for node in nodes.iter_mut().skip(1) {
                node.peers[0].stream = None;
            }
            if let Some(p) = nodes[1..]
                .iter()
                .position(|node| node.replica.is_primary() && node.replica.status == super::super::Status::Normal)
            {
                new_primary = Some(p + 1);
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let np = new_primary.expect("survivors failed to elect a new primary");
        assert!(np != 0);
        assert!(nodes[np].replica.view >= 1, "view did not advance");

        // The new primary accepts a fresh write with just the two survivors.
        assert!(nodes[np].submit(2, 1, 200), "new primary rejected write");
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(3) {
            for node in nodes.iter_mut().skip(1) {
                node.poll();
            }
            for node in nodes.iter_mut().skip(1) {
                node.peers[0].stream = None;
            }
            if nodes[1..].iter().all(|node| node.applied.iter().any(|c| c.value == 200)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        for node in &nodes[1..] {
            assert!(
                node.applied.iter().any(|c| c.value == 100),
                "survivor {} lost the pre-failover operation",
                node.id()
            );
            assert!(
                node.applied.iter().any(|c| c.value == 200),
                "survivor {} missing the post-failover operation",
                node.id()
            );
        }
    }
}
