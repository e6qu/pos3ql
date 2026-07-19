//! The VSR replica state machine.

use super::message::{LogEntry, Message, MessageBody, MAX_LOG};
use super::{primary_of, quorum, ReplicaId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Normal,
    ViewChange,
}

/// Bounded outbox filled during a step; the driver flushes it.
pub struct Outbox {
    msgs: [Message; OUTBOX_CAP],
    len: usize,
}

const OUTBOX_CAP: usize = 2 * MAX_LOG;

impl Outbox {
    fn new() -> Self {
        Self {
            msgs: [Message {
                from: 0,
                to: 0,
                body: MessageBody::Commit { view: 0, commit: 0 },
            }; OUTBOX_CAP],
            len: 0,
        }
    }

    fn push(&mut self, m: Message) {
        // Dropping on overflow is safe: VSR tolerates lost messages, and the
        // caller retransmits on the next tick. Never grows.
        if self.len < OUTBOX_CAP {
            self.msgs[self.len] = m;
            self.len += 1;
        }
    }

    pub fn drain(&mut self) -> impl Iterator<Item = Message> + '_ {
        let n = self.len;
        self.len = 0;
        self.msgs[..n].iter().copied()
    }
}

/// A committed operation delivered to the application, in commit order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Committed {
    pub operation: u64,
    pub client: u32,
    pub request: u32,
    pub value: u64,
}

pub struct Replica {
    pub id: ReplicaId,
    pub n: usize,
    pub view: u64,
    pub status: Status,
    pub operation: u64,
    pub commit: u64,
    /// Highest commit-number ever heard, even if our log was too short to
    /// apply it yet; applied as soon as the log catches up. This makes
    /// commit progress robust to message reordering.
    commit_max: u64,
    /// The view in which the log was last in Normal status (`log_view`).
    log_view: u64,
    log: [LogEntry; MAX_LOG],
    log_len: usize,

    // Primary quorum tracking for the current operation.
    prepare_ok_from: [bool; MAX_REPLICAS],
    prepare_ok_op: u64,

    // View-change vote tracking.
    start_view_change_from: [bool; MAX_REPLICAS],
    do_view_change_from: [bool; MAX_REPLICAS],
    do_view_change_msgs: [Option<DvcRecord>; MAX_REPLICAS],

    /// Ticks since the last message from the primary; triggers a view change.
    idle_ticks: u32,
    view_change_timeout: u32,
    /// Ticks since the primary last retransmitted uncommitted ops (rate
    /// limits recovery traffic so it cannot outrun delivery).
    ticks_since_resend: u32,

    outbox: Outbox,
    /// Committed ops delivered to the application this step.
    delivered: [Committed; MAX_LOG],
    delivered_len: usize,
}

pub const MAX_REPLICAS: usize = 7;

#[derive(Debug, Clone, Copy)]
struct DvcRecord {
    log_view: u64,
    operation: u64,
    commit: u64,
    log: [LogEntry; MAX_LOG],
    log_len: usize,
}

impl Replica {
    pub fn new(id: ReplicaId, n: usize, view_change_timeout: u32) -> Self {
        assert!((1..=MAX_REPLICAS).contains(&n), "cluster size out of range");
        Self {
            id,
            n,
            view: 0,
            status: Status::Normal,
            operation: 0,
            commit: 0,
            commit_max: 0,
            log_view: 0,
            log: [LogEntry::EMPTY; MAX_LOG],
            log_len: 0,
            prepare_ok_from: [false; MAX_REPLICAS],
            prepare_ok_op: 0,
            start_view_change_from: [false; MAX_REPLICAS],
            do_view_change_from: [false; MAX_REPLICAS],
            do_view_change_msgs: [None; MAX_REPLICAS],
            idle_ticks: 0,
            view_change_timeout,
            ticks_since_resend: 0,
            outbox: Outbox::new(),
            delivered: [Committed {
                operation: 0,
                client: 0,
                request: 0,
                value: 0,
            }; MAX_LOG],
            delivered_len: 0,
        }
    }

    /// Crash-restart with the durable journal intact (VSR persists view and
    /// log to disk). Only volatile protocol state is lost: quorum votes,
    /// timers, and any un-flushed outbox. The committed prefix and the log
    /// survive, which is exactly the fault model VSR tolerates.
    pub fn recover(&mut self) {
        self.prepare_ok_from = [false; MAX_REPLICAS];
        self.prepare_ok_op = 0;
        self.start_view_change_from = [false; MAX_REPLICAS];
        self.do_view_change_from = [false; MAX_REPLICAS];
        self.do_view_change_msgs = [None; MAX_REPLICAS];
        self.idle_ticks = 0;
        self.outbox = Outbox::new();
        self.delivered_len = 0;
        // A recovering replica does not act as primary until it re-learns
        // the current view from live traffic; drop to a following posture.
        if self.status == Status::Normal && self.is_primary() {
            self.status = Status::ViewChange;
        }
    }

    pub fn is_primary(&self) -> bool {
        primary_of(self.view, self.n) == self.id
    }

    /// Per-replica staggered timeout: the replica that would be primary of
    /// the next view times out first, so view changes converge instead of
    /// dueling. Spreads the rest by id to break symmetry.
    fn effective_timeout(&self) -> u32 {
        let next_primary = primary_of(self.view + 1, self.n) as u32;
        let rank = (self.id as u32 + self.n as u32 - next_primary) % self.n as u32;
        self.view_change_timeout + rank * (self.view_change_timeout / 2 + 1)
    }

    pub fn primary(&self) -> ReplicaId {
        primary_of(self.view, self.n)
    }

    pub fn outbox(&mut self) -> &mut Outbox {
        &mut self.outbox
    }

    /// Committed ops delivered during the last step, in order.
    pub fn take_delivered(&mut self) -> impl Iterator<Item = Committed> + '_ {
        let n = self.delivered_len;
        self.delivered_len = 0;
        self.delivered[..n].iter().copied()
    }

    /// A client request submitted at the primary. Returns whether it was
    /// accepted (only the Normal-status primary accepts).
    pub fn on_request(&mut self, client: u32, request: u32, value: u64) -> bool {
        if self.status != Status::Normal || !self.is_primary() {
            return false;
        }
        // Dedup: ignore a request already in the log for this client.
        if self
            .log_slice()
            .iter()
            .any(|e| e.client == client && e.request == request)
        {
            return true;
        }
        self.operation += 1;
        let entry = LogEntry {
            view: self.view,
            operation: self.operation,
            client,
            request,
            value,
        };
        self.append(entry);
        // Primary counts its own vote.
        self.reset_prepare_ok(self.operation);
        self.prepare_ok_from[self.id as usize] = true;
        self.for_each_peer(|this, peer| {
            this.outbox.push(Message {
                from: this.id,
                to: peer,
                body: MessageBody::Prepare {
                    view: this.view,
                    operation: this.operation,
                    commit: this.commit,
                    entry,
                },
            });
        });
        // A single-replica cluster commits immediately.
        self.maybe_commit_primary();
        true
    }

    /// A logical clock tick. Backups start a view change if the primary has
    /// gone silent; the primary sends periodic commits as heartbeats.
    pub fn on_tick(&mut self) {
        self.idle_ticks += 1;
        if self.is_primary() && self.status == Status::Normal {
            // Heartbeat so backups do not time out.
            self.for_each_peer(|this, peer| {
                this.outbox.push(Message {
                    from: this.id,
                    to: peer,
                    body: MessageBody::Commit {
                        view: this.view,
                        commit: this.commit,
                    },
                });
            });
            // Retransmit any uncommitted ops so a lost Prepare (or a lagging
            // backup) recovers without a separate state-transfer protocol.
            // Rate-limited so recovery traffic never outruns delivery.
            self.ticks_since_resend += 1;
            let resend_period = (self.view_change_timeout / 4).max(1);
            let (lo, hi) = if self.ticks_since_resend >= resend_period && self.operation > self.commit {
                self.ticks_since_resend = 0;
                (self.commit + 1, self.operation)
            } else {
                (1, 0) // empty range
            };
            for target_op in lo..=hi {
                if let Some(entry) = self.log_slice().iter().find(|e| e.operation == target_op).copied() {
                    let (view, commit) = (self.view, self.commit);
                    self.for_each_peer(|this, peer| {
                        this.outbox.push(Message {
                            from: this.id,
                            to: peer,
                            body: MessageBody::Prepare {
                                view,
                                operation: entry.operation,
                                commit,
                                entry,
                            },
                        });
                    });
                }
            }
            self.idle_ticks = 0;
        } else if self.n > 1 && self.idle_ticks >= self.effective_timeout() {
            self.begin_view_change(self.view + 1);
        }
    }

    pub fn on_message(&mut self, m: Message) {
        match m.body {
            MessageBody::Prepare {
                view,
                operation,
                commit,
                entry,
            } => self.on_prepare(m.from, view, operation, commit, entry),
            MessageBody::PrepareOk { view, operation } => self.on_prepare_ok(m.from, view, operation),
            MessageBody::Commit { view, commit } => self.on_commit_msg(view, commit),
            MessageBody::StartViewChange { view } => self.on_start_view_change(m.from, view),
            MessageBody::DoViewChange {
                view,
                log_view,
                operation,
                commit,
                log_len,
                log,
            } => self.on_do_view_change(m.from, view, log_view, operation, commit, log_len, log),
            MessageBody::StartView {
                view,
                operation,
                commit,
                log_len,
                log,
            } => self.on_start_view(view, operation, commit, log_len, log),
        }
    }

    // ---- normal operation ----

    fn on_prepare(&mut self, from: ReplicaId, view: u64, operation: u64, commit: u64, entry: LogEntry) {
        if view < self.view {
            return;
        }
        if view > self.view {
            // A newer view is in progress and we are behind; adopt it as a
            // normal-status follower once we see its traffic.
            self.enter_view_normal(view);
        }
        if self.status != Status::Normal {
            return;
        }
        self.idle_ticks = 0;
        // In-order append: the operation must directly follow ours. Acknowledge in
        // both cases — a fresh append and a retransmission of an operation we
        // already hold — so a lost PrepareOk is recovered on retransmit
        // (otherwise the primary can never reach a commit quorum).
        if operation == self.operation + 1 {
            self.append(entry);
            self.operation = operation;
        }
        if operation <= self.operation {
            self.outbox.push(Message {
                from: self.id,
                to: from,
                body: MessageBody::PrepareOk {
                    view: self.view,
                    operation,
                },
            });
        }
        // Advance commit up to what the primary reports (bounded by our log).
        self.advance_commit(commit);
    }

    fn on_prepare_ok(&mut self, from: ReplicaId, view: u64, operation: u64) {
        if view != self.view || self.status != Status::Normal || !self.is_primary() {
            return;
        }
        if operation != self.prepare_ok_op {
            return; // stale ack for an already-committed or unknown operation
        }
        self.prepare_ok_from[from as usize] = true;
        self.maybe_commit_primary();
    }

    fn maybe_commit_primary(&mut self) {
        let votes = self.prepare_ok_from[..self.n].iter().filter(|v| **v).count();
        if votes >= quorum(self.n) && self.prepare_ok_op > self.commit {
            self.advance_commit(self.prepare_ok_op);
            // Tell backups to commit.
            self.for_each_peer(|this, peer| {
                this.outbox.push(Message {
                    from: this.id,
                    to: peer,
                    body: MessageBody::Commit {
                        view: this.view,
                        commit: this.commit,
                    },
                });
            });
        }
    }

    fn on_commit_msg(&mut self, view: u64, commit: u64) {
        if view < self.view {
            return;
        }
        if view > self.view {
            self.enter_view_normal(view);
        }
        if self.status != Status::Normal {
            return;
        }
        self.idle_ticks = 0;
        self.advance_commit(commit);
    }

    // ---- view change ----

    fn begin_view_change(&mut self, new_view: u64) {
        if new_view <= self.view && self.status == Status::ViewChange {
            return;
        }
        self.view = new_view;
        self.status = Status::ViewChange;
        self.idle_ticks = 0;
        self.start_view_change_from = [false; MAX_REPLICAS];
        self.do_view_change_from = [false; MAX_REPLICAS];
        self.do_view_change_msgs = [None; MAX_REPLICAS];
        self.start_view_change_from[self.id as usize] = true;
        self.for_each_peer(|this, peer| {
            this.outbox.push(Message {
                from: this.id,
                to: peer,
                body: MessageBody::StartViewChange { view: new_view },
            });
        });
        // Also send our DoViewChange to the prospective primary immediately
        // in case a quorum of SVC already exists; harmless if early.
        self.maybe_send_do_view_change();
    }

    fn on_start_view_change(&mut self, from: ReplicaId, view: u64) {
        if view > self.view || (view == self.view && self.status == Status::Normal) {
            self.begin_view_change(view);
        }
        if view != self.view || self.status != Status::ViewChange {
            return;
        }
        self.start_view_change_from[from as usize] = true;
        self.maybe_send_do_view_change();
    }

    fn maybe_send_do_view_change(&mut self) {
        let votes = self.start_view_change_from[..self.n]
            .iter()
            .filter(|v| **v)
            .count();
        // f other SVCs plus our own = quorum; send DVC to the new primary.
        if votes >= quorum(self.n) {
            let target = self.primary();
            let msg = MessageBody::DoViewChange {
                view: self.view,
                log_view: self.log_view,
                operation: self.operation,
                commit: self.commit,
                log_len: self.log_len as u16,
                log: self.log,
            };
            if target == self.id {
                // Deliver to ourselves.
                self.on_do_view_change(
                    self.id,
                    self.view,
                    self.log_view,
                    self.operation,
                    self.commit,
                    self.log_len as u16,
                    self.log,
                );
            } else {
                self.outbox.push(Message {
                    from: self.id,
                    to: target,
                    body: msg,
                });
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn on_do_view_change(
        &mut self,
        from: ReplicaId,
        view: u64,
        log_view: u64,
        operation: u64,
        commit: u64,
        log_len: u16,
        log: [LogEntry; MAX_LOG],
    ) {
        if view < self.view {
            return;
        }
        if view > self.view {
            self.begin_view_change(view);
        }
        // Only the new primary collects DoViewChange.
        if primary_of(self.view, self.n) != self.id {
            return;
        }
        // The view change completes exactly once. Once we have installed the
        // new view's log and resumed Normal status, later (delayed or
        // duplicated) DoViewChange messages for this view must be ignored —
        // re-running selection here would discard ops the primary has since
        // accepted, splitting the log.
        if self.status == Status::Normal && self.log_view == self.view {
            return;
        }
        self.do_view_change_from[from as usize] = true;
        self.do_view_change_msgs[from as usize] = Some(DvcRecord {
            log_view,
            operation,
            commit,
            log,
            log_len: log_len as usize,
        });
        let votes = self.do_view_change_from[..self.n]
            .iter()
            .filter(|v| **v)
            .count();
        if votes < quorum(self.n) {
            return;
        }

        // Choose the log: the DVC with the largest log_view, breaking ties
        // by largest operation. This is VSR's "most up-to-date log" rule.
        let mut best: Option<DvcRecord> = None;
        let mut max_commit = 0u64;
        for rec in self.do_view_change_msgs[..self.n].iter().flatten() {
            max_commit = max_commit.max(rec.commit);
            best = Some(match best {
                None => *rec,
                Some(b) if (rec.log_view, rec.operation) > (b.log_view, b.operation) => *rec,
                Some(b) => b,
            });
        }
        let best = best.expect("quorum implies at least one record");
        self.log = best.log;
        self.log_len = best.log_len;
        self.operation = best.operation;
        self.log_view = self.view;
        self.status = Status::Normal;
        self.idle_ticks = 0;
        // Commit up to the highest known commit across the quorum.
        self.advance_commit(max_commit);
        // Reset prepare bookkeeping for any uncommitted tail.
        self.reset_prepare_ok(self.operation);
        self.prepare_ok_from[self.id as usize] = true;
        // Broadcast the new view's log.
        self.for_each_peer(|this, peer| {
            this.outbox.push(Message {
                from: this.id,
                to: peer,
                body: MessageBody::StartView {
                    view: this.view,
                    operation: this.operation,
                    commit: this.commit,
                    log_len: this.log_len as u16,
                    log: this.log,
                },
            });
        });
    }

    fn on_start_view(
        &mut self,
        view: u64,
        operation: u64,
        commit: u64,
        log_len: u16,
        log: [LogEntry; MAX_LOG],
    ) {
        if view < self.view || (view == self.view && self.status == Status::Normal) {
            return;
        }
        self.view = view;
        self.log = log;
        self.log_len = log_len as usize;
        self.operation = operation;
        self.log_view = view;
        self.status = Status::Normal;
        self.idle_ticks = 0;
        self.advance_commit(commit);
        // Acknowledge any uncommitted ops so the new primary can re-commit.
        if self.operation > self.commit {
            self.outbox.push(Message {
                from: self.id,
                to: self.primary(),
                body: MessageBody::PrepareOk {
                    view: self.view,
                    operation: self.operation,
                },
            });
        }
    }

    // ---- helpers ----

    fn enter_view_normal(&mut self, view: u64) {
        // Adopt a strictly newer view as a follower awaiting its log. We do
        // not accept ops until a StartView installs the authoritative log.
        self.view = view;
        self.status = Status::ViewChange;
        self.idle_ticks = 0;
    }

    fn append(&mut self, entry: LogEntry) {
        if self.log_len < MAX_LOG {
            self.log[self.log_len] = entry;
            self.log_len += 1;
        }
    }

    fn log_slice(&self) -> &[LogEntry] {
        &self.log[..self.log_len]
    }

    fn advance_commit(&mut self, target: u64) {
        if target > self.commit_max {
            self.commit_max = target;
        }
        let target = self.commit_max.min(self.operation);
        while self.commit < target {
            let next = self.commit + 1;
            // Deliver the operation at index `next` (ops are 1-based, log 0-based).
            if let Some(entry) = self.log_slice().iter().find(|e| e.operation == next) {
                if self.delivered_len < MAX_LOG {
                    self.delivered[self.delivered_len] = Committed {
                        operation: entry.operation,
                        client: entry.client,
                        request: entry.request,
                        value: entry.value,
                    };
                    self.delivered_len += 1;
                }
                self.commit = next;
            } else {
                break; // gap: cannot commit past a missing operation
            }
        }
    }

    fn reset_prepare_ok(&mut self, operation: u64) {
        self.prepare_ok_from = [false; MAX_REPLICAS];
        self.prepare_ok_op = operation;
    }

    /// Runs `f` for each peer id, without holding a borrow of self.
    fn for_each_peer(&mut self, mut f: impl FnMut(&mut Self, ReplicaId)) {
        for r in 0..self.n as ReplicaId {
            if r != self.id {
                f(self, r);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives a set of replicas synchronously with perfect delivery until
    /// the network is quiet, then returns.
    fn settle(replicas: &mut [Replica]) {
        let mut queue: Vec<Message> = Vec::new();
        for r in replicas.iter_mut() {
            queue.extend(r.outbox().drain());
        }
        let mut steps = 0;
        while let Some(m) = queue.pop() {
            steps += 1;
            assert!(steps < 100_000, "did not settle");
            let to = m.to as usize;
            replicas[to].on_message(m);
            let mut out = Vec::new();
            out.extend(replicas[to].outbox().drain());
            for r in replicas.iter_mut() {
                let _ = r.take_delivered();
            }
            queue.extend(out);
        }
    }

    fn cluster(n: usize) -> Vec<Replica> {
        (0..n).map(|i| Replica::new(i as u8, n, 10)).collect()
    }

    #[test]
    fn single_replica_commits_immediately() {
        let mut c = cluster(1);
        assert!(c[0].on_request(1, 1, 42));
        assert_eq!(c[0].commit, 1);
        let delivered: Vec<_> = c[0].take_delivered().collect();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].value, 42);
    }

    #[test]
    fn three_replicas_commit_via_quorum() {
        let mut c = cluster(3);
        assert!(c[0].on_request(1, 1, 100));
        settle(&mut c);
        // All replicas commit the operation.
        for r in &c {
            assert_eq!(r.commit, 1, "replica {} did not commit", r.id);
            assert_eq!(r.operation, 1);
        }
    }

    #[test]
    fn backup_rejects_client_requests() {
        let mut c = cluster(3);
        assert!(!c[1].on_request(1, 1, 5), "backup must reject");
    }

    #[test]
    fn duplicate_request_is_deduped() {
        let mut c = cluster(3);
        c[0].on_request(7, 1, 10);
        settle(&mut c);
        let before = c[0].operation;
        c[0].on_request(7, 1, 10); // same client+request
        assert_eq!(c[0].operation, before, "duplicate must not advance operation");
    }

    #[test]
    fn view_change_elects_new_primary_and_preserves_log() {
        let mut c = cluster(3);
        // Commit one operation under view 0 (primary 0).
        c[0].on_request(1, 1, 55);
        settle(&mut c);
        assert!(c.iter().all(|r| r.commit == 1));

        // Primary 0 goes silent: backups 1 and 2 time out.
        for _ in 0..12 {
            c[1].on_tick();
            c[2].on_tick();
        }
        // Exchange view-change messages among 1 and 2 (0 is "down").
        let mut queue: Vec<Message> = Vec::new();
        for id in [1usize, 2] {
            queue.extend(c[id].outbox().drain().filter(|m| m.to != 0));
        }
        let mut steps = 0;
        while let Some(m) = queue.pop() {
            steps += 1;
            assert!(steps < 100_000);
            if m.to == 0 {
                continue; // primary 0 is down
            }
            c[m.to as usize].on_message(m);
            let out: Vec<_> = c[m.to as usize].outbox().drain().filter(|m| m.to != 0).collect();
            queue.extend(out);
        }
        // New view is 1, primary is replica 1, and the committed operation survives.
        assert_eq!(c[1].view, 1);
        assert!(c[1].is_primary());
        assert_eq!(c[1].status, Status::Normal);
        assert!(c[1].commit >= 1, "committed operation lost across view change");
        assert!(c[2].commit >= 1);

        // The new primary can accept and commit a fresh operation with only 1 and 2.
        assert!(c[1].on_request(2, 1, 66));
        let mut queue: Vec<Message> = c[1].outbox().drain().filter(|m| m.to != 0).collect();
        let mut steps = 0;
        while let Some(m) = queue.pop() {
            steps += 1;
            assert!(steps < 100_000);
            if m.to == 0 {
                continue;
            }
            c[m.to as usize].on_message(m);
            let out: Vec<_> = c[m.to as usize].outbox().drain().filter(|m| m.to != 0).collect();
            queue.extend(out);
        }
        assert_eq!(c[1].commit, 2, "new primary failed to commit after view change");
    }
}
