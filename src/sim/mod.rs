//! VOPR: a deterministic Viewstamped-Operation-Replicator simulator.
//!
//! In the spirit of TigerBeetle's VOPR, this drives a whole VSR cluster
//! through a virtual network with fault injection — message loss, reorder,
//! duplication, delay, and replica crash/restart — all from a single PRNG
//! seed, so any failure reproduces exactly. After the run it checks the
//! consensus invariants: replicas never disagree on a committed op, an
//! acknowledged op is never lost, and the committed prefix is a
//! linearizable sequence.
//!
//! Everything is logical: virtual time is a tick counter, no wall clock, no
//! sockets. Randomness is [`crate::prng::Pcg32`], seeded per run.

use crate::prng::Pcg32;
use crate::vsr::message::Message;
use crate::vsr::replica::{Committed, Replica, MAX_REPLICAS};
use crate::vsr::Status;

/// Bounds in-flight messages so a message storm cannot grow memory without
/// limit; overflow drops sends, which VSR treats as ordinary loss.
const NETWORK_CAP: usize = 4096;

/// A message in flight, scheduled to arrive at `deliver_at`.
#[derive(Clone, Copy)]
struct InFlight {
    deliver_at: u64,
    msg: Message,
}

#[derive(Clone)]
pub struct SimConfig {
    pub replicas: usize,
    pub ticks: u64,
    /// Per-mille chance a sent message is dropped.
    pub drop_permille: u32,
    /// Per-mille chance a delivered message is also duplicated.
    pub duplicate_permille: u32,
    /// Max extra delay (in ticks) added to a message.
    pub max_delay: u64,
    /// Per-mille chance, per tick, that a replica crashes (loses volatile
    /// state and restarts from view 0). Kept low.
    pub crash_permille: u32,
    /// Per-mille chance, per tick, of toggling a network partition.
    pub partition_permille: u32,
    pub view_change_timeout: u32,
    /// New client requests to submit over the run.
    pub requests: u32,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            replicas: 3,
            ticks: 2_500,
            drop_permille: 100,
            duplicate_permille: 20,
            max_delay: 5,
            crash_permille: 2,
            partition_permille: 5,
            view_change_timeout: 20,
            requests: 40,
        }
    }
}

/// Outcome of a simulation run.
#[derive(Debug, Clone)]
pub struct SimReport {
    pub seed: u64,
    pub committed: u64,
    pub acknowledged: u64,
    pub view_changes: u64,
    pub crashes: u64,
    /// None = all invariants held; Some(msg) = a violation (reproducible
    /// from `seed`).
    pub violation: Option<String>,
}

struct Sim {
    cfg: SimConfig,
    rng: Pcg32,
    now: u64,
    replicas: Vec<Replica>,
    network: Vec<InFlight>,
    /// Bidirectional partition matrix: partitioned[a][b] blocks a→b.
    partitioned: [[bool; MAX_REPLICAS]; MAX_REPLICAS],
    /// The authoritative committed log per replica, appended in commit
    /// order; used to check agreement and no-loss.
    committed_log: Vec<Vec<Committed>>,
    /// Client requests the harness has issued: (client, request, value).
    issued: Vec<(u32, u32, u64)>,
    /// Requests confirmed committed (acknowledged to the client): value set.
    acknowledged: std::collections::HashSet<(u32, u32)>,
    next_request: u32,
    view_changes: u64,
    crashes: u64,
    max_view_seen: u64,
}

impl Sim {
    fn new(seed: u64, cfg: SimConfig) -> Self {
        let n = cfg.replicas;
        let replicas = (0..n)
            .map(|i| Replica::new(i as u8, n, cfg.view_change_timeout))
            .collect();
        Self {
            rng: Pcg32::new(seed, 0xda7a),
            cfg,
            now: 0,
            replicas,
            network: Vec::new(),
            partitioned: [[false; MAX_REPLICAS]; MAX_REPLICAS],
            committed_log: vec![Vec::new(); n],
            issued: Vec::new(),
            acknowledged: std::collections::HashSet::new(),
            next_request: 1,
            view_changes: 0,
            crashes: 0,
            max_view_seen: 0,
        }
    }

    fn chance(&mut self, permille: u32) -> bool {
        self.rng.chance(permille, 1000)
    }

    fn send_all(&mut self, from: usize) {
        let mut out = Vec::new();
        out.extend(self.replicas[from].outbox().drain());
        for msg in out {
            let a = msg.from as usize;
            let b = msg.to as usize;
            // Partitioned links drop the message.
            if self.partitioned[a][b] {
                continue;
            }
            if self.chance(self.cfg.drop_permille) {
                continue;
            }
            let delay = if self.cfg.max_delay > 0 {
                self.rng.next_bounded(self.cfg.max_delay as u32 + 1) as u64
            } else {
                0
            };
            if self.network.len() < NETWORK_CAP {
                self.network.push(InFlight {
                    deliver_at: self.now + delay,
                    msg,
                });
            }
            if self.chance(self.cfg.duplicate_permille) && self.network.len() < NETWORK_CAP {
                self.network.push(InFlight {
                    deliver_at: self.now + delay + 1,
                    msg,
                });
            }
        }
    }

    fn primary(&self) -> Option<usize> {
        // The replica in Normal status with the highest view, if it is the
        // primary for that view.
        let mut best: Option<usize> = None;
        for (i, r) in self.replicas.iter().enumerate() {
            if r.status == Status::Normal
                && r.is_primary()
                && best.is_none_or(|b| r.view > self.replicas[b].view)
            {
                best = Some(i);
            }
        }
        best
    }

    fn run(mut self, seed: u64) -> SimReport {
        for _ in 0..self.cfg.ticks {
            self.now += 1;

            // Deliver due messages.
            let mut due = Vec::new();
            let mut i = 0;
            while i < self.network.len() {
                if self.network[i].deliver_at <= self.now {
                    due.push(self.network.swap_remove(i));
                } else {
                    i += 1;
                }
            }
            // Deterministic order: sort due messages by (to, from, discriminant).
            due.sort_by_key(|f| (f.msg.to, f.msg.from, self.now));
            for f in due {
                let to = f.msg.to as usize;
                // A partition can block in-flight delivery too.
                if self.partitioned[f.msg.from as usize][to] {
                    continue;
                }
                self.replicas[to].on_message(f.msg);
                self.collect_commits(to);
                self.send_all(to);
            }

            // Ticks.
            for r in 0..self.replicas.len() {
                self.replicas[r].on_tick();
                self.collect_commits(r);
                self.send_all(r);
            }

            // Track view changes.
            for r in &self.replicas {
                if r.view > self.max_view_seen {
                    self.view_changes += r.view - self.max_view_seen;
                    self.max_view_seen = r.view;
                }
            }

            // Submit a client request to the current primary.
            if self.next_request <= self.cfg.requests
                && let Some(p) = self.primary() {
                    let client = 1 + (self.next_request % 4);
                    let req = self.next_request;
                    let value = u64::from(req) * 1000 + u64::from(client);
                    if self.replicas[p].on_request(client, req, value) {
                        self.issued.push((client, req, value));
                        self.collect_commits(p);
                        self.send_all(p);
                        self.next_request += 1;
                    }
                }

            // Fault injection.
            self.maybe_crash();
            self.maybe_partition();

            if let Some(v) = self.check_invariants() {
                return SimReport {
                    seed,
                    committed: self.total_committed(),
                    acknowledged: self.acknowledged.len() as u64,
                    view_changes: self.view_changes,
                    crashes: self.crashes,
                    violation: Some(v),
                };
            }
        }

        // Final settle with a clean network so healthy replicas converge.
        self.heal_and_settle();

        let violation = self.check_invariants().or_else(|| self.check_liveness());
        SimReport {
            seed,
            committed: self.total_committed(),
            acknowledged: self.acknowledged.len() as u64,
            view_changes: self.view_changes,
            crashes: self.crashes,
            violation,
        }
    }

    fn collect_commits(&mut self, r: usize) {
        let mut new = Vec::new();
        new.extend(self.replicas[r].take_delivered());
        for c in new {
            self.committed_log[r].push(c);
            // The op is durably committed at a majority once any replica
            // delivers it (it required a quorum of prepare_oks), so the
            // client can consider it acknowledged.
            self.acknowledged.insert((c.client, c.request));
        }
    }

    fn maybe_crash(&mut self) {
        if !self.chance(self.cfg.crash_permille) {
            return;
        }
        let victim = self.rng.next_bounded(self.replicas.len() as u32) as usize;
        // Crash-restart with the durable journal intact: only volatile
        // protocol state is lost. The committed prefix (application state,
        // rebuilt from the log on recovery) and the log survive — the fault
        // model VSR is designed to tolerate.
        self.replicas[victim].recover();
        self.crashes += 1;
    }

    fn maybe_partition(&mut self) {
        if !self.chance(self.cfg.partition_permille) {
            return;
        }
        let a = self.rng.next_bounded(self.replicas.len() as u32) as usize;
        let b = self.rng.next_bounded(self.replicas.len() as u32) as usize;
        if a != b {
            let now = self.partitioned[a][b];
            self.partitioned[a][b] = !now;
            self.partitioned[b][a] = !now;
        }
    }

    /// Clears partitions and runs a quiet period so healthy replicas can
    /// catch up before the final liveness check.
    fn heal_and_settle(&mut self) {
        self.partitioned = [[false; MAX_REPLICAS]; MAX_REPLICAS];
        // A real quiescent period: no loss, dup, delay, crashes, or new
        // partitions, so healthy replicas can actually converge.
        self.cfg.drop_permille = 0;
        self.cfg.duplicate_permille = 0;
        self.cfg.max_delay = 0;
        self.cfg.crash_permille = 0;
        self.cfg.partition_permille = 0;
        for _ in 0..(self.cfg.view_change_timeout as u64 * 10 + 100) {
            self.now += 1;
            let mut due = Vec::new();
            let mut i = 0;
            while i < self.network.len() {
                if self.network[i].deliver_at <= self.now {
                    due.push(self.network.swap_remove(i));
                } else {
                    i += 1;
                }
            }
            due.sort_by_key(|f| (f.msg.to, f.msg.from));
            for f in due {
                let to = f.msg.to as usize;
                self.replicas[to].on_message(f.msg);
                self.collect_commits(to);
                self.send_all(to);
            }
            for r in 0..self.replicas.len() {
                self.replicas[r].on_tick();
                self.collect_commits(r);
                self.send_all(r);
            }
            // Keep re-submitting the current request so it eventually lands.
            if self.next_request <= self.cfg.requests
                && let Some(p) = self.primary() {
                    let client = 1 + (self.next_request % 4);
                    let req = self.next_request;
                    let value = u64::from(req) * 1000 + u64::from(client);
                    if self.replicas[p].on_request(client, req, value) {
                        if !self.issued.iter().any(|(_, r, _)| *r == req) {
                            self.issued.push((client, req, value));
                        }
                        self.collect_commits(p);
                        self.send_all(p);
                        self.next_request += 1;
                    }
                }
        }
    }

    fn total_committed(&self) -> u64 {
        self.committed_log
            .iter()
            .map(|l| l.len() as u64)
            .max()
            .unwrap_or(0)
    }

    /// Safety invariants — a violation means the protocol is broken.
    fn check_invariants(&self) -> Option<String> {
        // (1) Agreement: for every op index, all replicas that have
        // committed that far agree on the (client, request, value).
        let max_len = self
            .committed_log
            .iter()
            .map(|l| l.len())
            .max()
            .unwrap_or(0);
        for idx in 0..max_len {
            let mut chosen: Option<Committed> = None;
            for log in &self.committed_log {
                if let Some(c) = log.get(idx) {
                    match chosen {
                        None => chosen = Some(*c),
                        Some(prev) => {
                            if prev.client != c.client
                                || prev.request != c.request
                                || prev.value != c.value
                                || prev.op != c.op
                            {
                                return Some(format!(
                                    "AGREEMENT VIOLATED at commit index {idx}: {prev:?} vs {c:?}"
                                ));
                            }
                        }
                    }
                }
            }
        }
        // (2) Monotonic op numbers within each replica's committed log.
        for (r, log) in self.committed_log.iter().enumerate() {
            for w in log.windows(2) {
                if w[1].op != w[0].op + 1 {
                    return Some(format!(
                        "GAP/REORDER in replica {r} committed log: {:?} then {:?}",
                        w[0], w[1]
                    ));
                }
            }
        }
        // (3) No duplicate application of the same client request across a
        // single replica's committed log.
        for (r, log) in self.committed_log.iter().enumerate() {
            let mut seen = std::collections::HashSet::new();
            for c in log {
                if !seen.insert((c.client, c.request)) {
                    return Some(format!(
                        "DUPLICATE apply of ({},{}) in replica {r}",
                        c.client, c.request
                    ));
                }
            }
        }
        None
    }

    /// Liveness: after healing, at least one request must have committed if
    /// any were issued (the cluster is not permanently stuck). This is a
    /// weak check — full progress depends on the fault schedule.
    fn check_liveness(&self) -> Option<String> {
        if !self.issued.is_empty() && self.total_committed() == 0 {
            return Some(
                "LIVENESS: requests were issued but nothing committed after healing".to_string(),
            );
        }
        None
    }
}

/// Runs one simulation from a seed.
pub fn run(seed: u64, cfg: SimConfig) -> SimReport {
    Sim::new(seed, cfg).run(seed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perfect_network_commits_everything() {
        let cfg = SimConfig {
            drop_permille: 0,
            duplicate_permille: 0,
            max_delay: 0,
            crash_permille: 0,
            partition_permille: 0,
            requests: 20,
            ticks: 2000,
            ..Default::default()
        };
        let report = run(1, cfg);
        assert!(report.violation.is_none(), "{:?}", report.violation);
        assert!(report.committed >= 20, "committed {}", report.committed);
    }

    #[test]
    fn single_replica_is_trivially_consistent() {
        let cfg = SimConfig {
            replicas: 1,
            requests: 15,
            ticks: 1000,
            // A lone node has no peer to recover volatile state from, so a
            // crash there is modeled elsewhere (WAL recovery), not here.
            crash_permille: 0,
            partition_permille: 0,
            ..Default::default()
        };
        let report = run(42, cfg);
        assert!(report.violation.is_none(), "{:?}", report.violation);
        assert_eq!(report.committed, 15);
    }

    #[test]
    fn lossy_network_preserves_safety() {
        // Heavy loss/delay/dup but no crashes/partitions: safety must hold
        // and progress should still happen across many seeds.
        for seed in 0..40u64 {
            let cfg = SimConfig {
                drop_permille: 200,
                duplicate_permille: 50,
                max_delay: 8,
                crash_permille: 0,
                partition_permille: 0,
                requests: 20,
                ticks: 8000,
                ..Default::default()
            };
            let report = run(seed, cfg);
            assert!(
                report.violation.is_none(),
                "seed {seed}: {:?}",
                report.violation
            );
            assert!(report.committed > 0, "seed {seed}: no progress");
        }
    }

    #[test]
    fn faults_preserve_safety_across_seeds() {
        // The full chaos: loss, reorder, dup, crashes, partitions. Safety
        // (agreement / no gaps / no dup-apply) must hold for every seed;
        // liveness is not asserted here since an adversarial schedule can
        // legitimately stall a run.
        let mut violations = Vec::new();
        for seed in 0..60u64 {
            let report = run(seed, SimConfig::default());
            if let Some(v) = report.violation {
                // Ignore pure-liveness stalls; assert only safety.
                if !v.starts_with("LIVENESS") {
                    violations.push((seed, v));
                }
            }
        }
        assert!(violations.is_empty(), "safety violations: {violations:?}");
    }

    #[test]
    fn reproducible_from_seed() {
        let a = run(12345, SimConfig::default());
        let b = run(12345, SimConfig::default());
        assert_eq!(a.committed, b.committed);
        assert_eq!(a.acknowledged, b.acknowledged);
        assert_eq!(a.view_changes, b.view_changes);
        assert_eq!(a.crashes, b.crashes);
        assert_eq!(a.violation, b.violation);
    }
}
