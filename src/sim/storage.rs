//! The storage VOPR: the whole storage stack — WAL, spill, checkpoint,
//! block SSTs, cache tiers, manifest CAS, garbage sweep — driven through the
//! real [`Engine`] against the deterministic virtual bucket
//! ([`crate::s3::sim`]), under seeded fault schedules: transient request
//! failures, ambiguous PUTs, flipped bits on the wire, outages that begin
//! mid-operation and end in a crash, corrupted disk-cache slots, warm
//! restarts and wiped-disk cold starts.
//!
//! A model database (plain maps) tracks what a client was told. The
//! invariants, checked after every recovery and at every verification point:
//!
//! - an acknowledged write is never lost and never altered — recovered
//!   state equals the model exactly;
//! - a write whose acknowledgement was lost (a statement or COMMIT that
//!   returned an error while faults were live) is *uncertain*: afterwards
//!   the engine must show either the before or the intended state, and
//!   whichever it shows is adopted as truth — never a third value;
//! - a wiped-disk cold start taken after a clean CHECKPOINT reproduces the
//!   model exactly (nothing was living only on the local disk);
//! - the bucket never sees an unconditional overwrite that changes an
//!   object's bytes (blocks are content-addressed, segments write-once, the
//!   manifest moves only by CAS) — recorded by the bucket itself;
//! - corruption is loud: a flipped bit or a scribbled cache slot may fail a
//!   statement, but a *successful* verification is always exact.
//!
//! Every choice is drawn from one PCG stream per seed, so a failure
//! reproduces from its seed alone: `POS3QL_STORAGE_VOPR_SEED0`, with
//! `POS3QL_STORAGE_VOPR_SEEDS` and `POS3QL_STORAGE_VOPR_STEPS` scaling the
//! sweep past the checked-in defaults.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use crate::config::Config;
use crate::mem::arena::Arena;
use crate::mem::budget::Budget;
use crate::mem::buffer::FixedBuf;
use crate::pg::respond::Responder;
use crate::prng::Pcg32;
use crate::s3::sim::{drop_bucket, open_bucket, SimBucket};
use crate::sql::cursor::CursorPool;
use crate::sql::guc::GucState;
use crate::sql::prep::SqlPreparedPool;
use crate::sql::txn::TxnState;
use crate::sql::Engine;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    v: i64,
    pad: String,
}

/// What a client saw for one statement: the data rows, the rows-affected
/// count from the CommandComplete tag, or the first error.
struct Outcome {
    rows: Vec<(i64, Row)>,
    affected: Option<u64>,
    error: Option<(String, String)>,
}

impl Outcome {
    fn ok(&self) -> bool {
        self.error.is_none()
    }
}

/// One engine incarnation with its per-connection state; dropped whole to
/// simulate a crash.
struct Session {
    engine: Engine,
    txn: TxnState,
    prepared: SqlPreparedPool,
    cursors: CursorPool,
    guc: GucState,
    send: FixedBuf,
    arena: Arena,
}

struct World {
    seed: u64,
    rng: Pcg32,
    config: Config,
    bucket: Rc<RefCell<SimBucket>>,
    session: Option<Session>,
    /// Committed state a client was told about, id → row.
    model: BTreeMap<i64, Row>,
    /// Writes whose acknowledgement was lost: id → (before, intended).
    uncertain: BTreeMap<i64, (Option<Row>, Option<Row>)>,
    next_id: i64,
    steps_taken: u64,
}

fn vopr_config(seed: u64) -> Config {
    let mut config = Config::default_dev();
    let dir = std::env::temp_dir().join(format!(
        "pos3ql-storage-vopr-{}-{seed}",
        std::process::id()
    ));
    config.data_dir = dir.to_str().unwrap().to_string();
    config.s3_on = true;
    config.s3_sim = true;
    config.s3_bucket = format!("vopr-{}-{seed}", std::process::id());
    config.s3_response_bytes = 1 << 20;
    config.wal_upload = true;
    config.wal_upload_sync = true;
    config.wal_upload_buffer_bytes = 256 * 1024;
    config.wal_bytes = 8 << 20;
    config.wal_buffer_bytes = 64 * 1024;
    // Small enough that the workload spills and checkpoints on its own.
    config.memtable_bytes = 256 * 1024;
    config.block_cache_bytes = 512 * 1024;
    config.disk_cache_bytes = 1 << 20;
    config.max_tables = 4;
    config.table_rows = 4096;
    config.txn_rows = 1024;
    config.max_prepared = 4;
    config.prepared_bytes = 1024;
    config.max_cursors = 2;
    config.cursor_bytes = 16 * 1024;
    config.sql_arena_bytes = 256 * 1024;
    config.work_arena_bytes = 2 << 20;
    config
}

impl World {
    fn new(seed: u64) -> Self {
        let config = vopr_config(seed);
        let _ = std::fs::remove_dir_all(&config.data_dir);
        drop_bucket(&config.s3_bucket);
        let bucket = open_bucket(&config.s3_bucket, seed);
        let mut world = Self {
            seed,
            rng: Pcg32::new(seed, 0x5709a6e), // storage-VOPR stream
            config,
            bucket,
            session: None,
            model: BTreeMap::new(),
            uncertain: BTreeMap::new(),
            next_id: 1,
            steps_taken: 0,
        };
        world.start_engine();
        let created = world.run("CREATE TABLE t (id bigint PRIMARY KEY, v bigint, pad text)");
        assert!(created.ok(), "seed {seed}: setup failed: {:?}", created.error);
        world
    }

    fn start_engine(&mut self) {
        let mut budget = Budget::new(1 << 28);
        let engine = Engine::new(&self.config, &mut budget)
            .unwrap_or_else(|e| panic!("seed {}: engine start failed: {e}", self.seed));
        self.session = Some(Session {
            engine,
            txn: TxnState::new(&mut budget, self.config.txn_rows).unwrap(),
            prepared: SqlPreparedPool::new(&self.config, &mut budget).unwrap(),
            cursors: CursorPool::new(&self.config, &mut budget).unwrap(),
            guc: GucState::new(),
            send: FixedBuf::new(&mut budget, "vopr send", 1 << 20).unwrap(),
            arena: Arena::new(&mut budget, "vopr sql", 1 << 18).unwrap(),
        });
    }

    /// Runs one simple-protocol statement and reads back what a client saw.
    fn run(&mut self, sql: &str) -> Outcome {
        let trace = std::env::var("POS3QL_VOPR_TRACE").is_ok();
        let session = self.session.as_mut().expect("engine is up");
        session.send.clear();
        session.arena.reset();
        let mut responder = Responder::new(&mut session.send);
        let sent = session.engine.execute_simple(
            sql,
            &session.arena,
            &mut session.txn,
            &mut session.prepared,
            &mut session.cursors,
            &mut session.guc,
            &mut responder,
        );
        assert!(sent.is_ok(), "seed {}: send buffer overflow on: {sql}", self.seed);
        // What the connection loop does after every query message: the
        // auto-checkpoint rides here, so it runs under fault fire too (its
        // failures go to stderr and the next message retries).
        session.engine.maybe_checkpoint();
        let outcome = parse_outcome(session.send.readable());
        if trace {
            let head: String = sql.chars().take(60).collect();
            eprintln!(
                "[vopr {}] {head} -> {}",
                self.steps_taken,
                match &outcome.error {
                    Some((code, message)) => format!("ERROR {code}: {message}"),
                    None => format!("ok ({} rows)", outcome.rows.len()),
                }
            );
        }
        outcome
    }

    fn faults(&self) -> std::cell::RefMut<'_, SimBucket> {
        self.bucket.borrow_mut()
    }

    fn clear_faults(&mut self) {
        let mut bucket = self.faults();
        bucket.faults.fail_from_op = None;
        bucket.faults.transient_per_mille = 0;
        bucket.faults.ambiguous_put_per_mille = 0;
        bucket.faults.rot_per_mille = 0;
    }

    /// A DML target whose state is settled. An id with an unresolved write
    /// is off limits: a later acknowledged write over it would make *both*
    /// candidate histories stale, and the model tracks only two.
    fn pick_certain_id(&mut self) -> Option<i64> {
        let candidates: Vec<i64> = self
            .model
            .keys()
            .filter(|id| !self.uncertain.contains_key(id))
            .copied()
            .collect();
        if candidates.is_empty() {
            return None;
        }
        Some(candidates[self.rng.next_bounded(candidates.len() as u32) as usize])
    }

    fn fresh_row(&mut self) -> Row {
        let v = self.rng.next_u32() as i64;
        let pad_len = 40 + self.rng.next_bounded(200) as usize;
        let mut pad = String::with_capacity(pad_len);
        for _ in 0..pad_len {
            pad.push((b'a' + (self.rng.next_bounded(26) as u8)) as char);
        }
        Row { v, pad }
    }

    /// One batch of DML — autocommit or a transaction — applying its effect
    /// to the model exactly as far as the engine acknowledged it.
    fn dml_burst(&mut self) {
        let in_txn = self.rng.next_bounded(10) < 4;
        let statements = 1 + self.rng.next_bounded(5);
        let mut pending: BTreeMap<i64, (Option<Row>, Option<Row>)> = BTreeMap::new();
        if in_txn {
            let begun = self.run("BEGIN");
            if !begun.ok() {
                panic!("seed {}: BEGIN failed: {:?}", self.seed, begun.error);
            }
        }
        let mut aborted = false;
        for _ in 0..statements {
            // Bias toward deletes when the table is getting full, so row
            // count stays inside the fixed pools.
            let live = self.model.len();
            let kind = if live < 20 {
                0
            } else if live > 500 {
                1 + self.rng.next_bounded(2)
            } else {
                self.rng.next_bounded(3)
            };
            let (sql, id, before, intended) = match kind {
                0 => {
                    let id = self.next_id;
                    self.next_id += 1;
                    let row = self.fresh_row();
                    (
                        format!(
                            "INSERT INTO t VALUES ({id}, {}, '{}')",
                            row.v, row.pad
                        ),
                        id,
                        None,
                        Some(row),
                    )
                }
                1 => {
                    let Some(id) = self.pick_certain_id() else { continue };
                    (
                        format!("DELETE FROM t WHERE id = {id}"),
                        id,
                        self.model.get(&id).cloned(),
                        None,
                    )
                }
                _ => {
                    let Some(id) = self.pick_certain_id() else { continue };
                    let row = self.fresh_row();
                    (
                        format!(
                            "UPDATE t SET v = {}, pad = '{}' WHERE id = {id}",
                            row.v, row.pad
                        ),
                        id,
                        self.model.get(&id).cloned(),
                        Some(row),
                    )
                }
            };
            // A transactional statement already touched by this burst keeps
            // its original before-image.
            let before = pending
                .get(&id)
                .map(|(b, _)| b.clone())
                .unwrap_or(before);
            let outcome = self.run(&sql);
            if outcome.ok() {
                // Trust the CommandComplete tag: a statement that touched no
                // row (an UPDATE of an id this same transaction deleted)
                // changes nothing.
                if outcome.affected == Some(0) {
                    continue;
                }
                if in_txn {
                    pending.insert(id, (before, intended));
                } else {
                    apply(&mut self.model, id, intended);
                }
            } else if in_txn {
                // The transaction is aborted; nothing in it survives.
                aborted = true;
                break;
            } else {
                // An autocommit statement that errored is an unknown
                // outcome: durable locally or in the bucket, or nowhere.
                self.uncertain.insert(id, (before, intended));
            }
        }
        if in_txn {
            if aborted || self.rng.next_bounded(10) < 2 {
                let rolled = self.run("ROLLBACK");
                assert!(
                    rolled.ok(),
                    "seed {}: ROLLBACK failed: {:?}",
                    self.seed,
                    rolled.error
                );
            } else {
                let committed = self.run("COMMIT");
                if committed.ok() {
                    for (id, (_, intended)) in pending {
                        apply(&mut self.model, id, intended);
                    }
                } else {
                    // The commit's acknowledgement was lost; every row it
                    // touched is now uncertain between its images.
                    for (id, images) in pending {
                        self.uncertain.insert(id, images);
                    }
                }
            }
        }
    }

    /// A CHECKPOINT is allowed to fail under faults; it must never change
    /// SQL-visible state either way.
    fn checkpoint(&mut self, must_succeed: bool) {
        let outcome = self.run("CHECKPOINT");
        if must_succeed {
            assert!(
                outcome.ok(),
                "seed {}: fault-free CHECKPOINT failed: {:?}",
                self.seed,
                outcome.error
            );
        }
    }

    /// Full verification: exact model equality, uncertainty resolution, and
    /// the bucket's own watchers. Runs fault-free.
    fn verify(&mut self, context: &str) {
        self.clear_faults();
        let outcome = self.run("SELECT id, v, pad FROM t ORDER BY id");
        assert!(
            outcome.ok(),
            "seed {} [{context}]: fault-free SELECT failed: {:?}",
            self.seed,
            outcome.error
        );
        let observed: BTreeMap<i64, Row> = outcome.rows.into_iter().collect();
        // Resolve every uncertain write by observation: the engine must
        // show one of the two images, and its answer becomes the truth.
        let uncertain = std::mem::take(&mut self.uncertain);
        for (id, (before, intended)) in uncertain {
            let seen = observed.get(&id).cloned();
            if seen == before {
                apply(&mut self.model, id, before);
            } else if seen == intended {
                apply(&mut self.model, id, intended);
            } else {
                panic!(
                    "seed {} [{context}]: uncertain id {id} resolved to a third \
                     value: saw {seen:?}, expected {before:?} or {intended:?}",
                    self.seed
                );
            }
        }
        assert_eq!(
            observed, self.model,
            "seed {} [{context}]: recovered state diverges from the model",
            self.seed
        );
        let blind = self.bucket.borrow().blind_overwrites.clone();
        assert!(
            blind.is_empty(),
            "seed {} [{context}]: blind overwrites changed object bytes: {blind:?}",
            self.seed
        );
    }

    /// Drops the engine where it stands — in-RAM state, open transaction
    /// and all — and starts a new incarnation on the same disk and bucket.
    fn crash_and_restart(&mut self, context: &str) {
        self.session = None;
        self.clear_faults();
        // A crash never commits: whatever a transaction had pending is gone.
        // (Uncertain entries stay uncertain — recovery resolves them.)
        if self.rng.next_bounded(2) == 0 {
            self.corrupt_disk_cache();
        }
        self.start_engine();
        self.verify(context);
    }

    /// Scribbles over the disk-cache file while the engine is down. The
    /// cache is pure cache: every scribbled slot must read as a miss, never
    /// as data.
    fn corrupt_disk_cache(&mut self) {
        let path = std::path::Path::new(&self.config.data_dir).join("block-cache");
        let Ok(mut bytes) = std::fs::read(&path) else { return };
        if bytes.is_empty() {
            return;
        }
        for _ in 0..64 {
            let at = self.rng.next_bounded(bytes.len() as u32) as usize;
            bytes[at] ^= 0xA5;
        }
        std::fs::write(&path, &bytes).expect("rewrite cache file");
    }

    /// The strongest recovery: checkpoint cleanly, wipe the local disk, and
    /// come back with nothing but the bucket.
    fn cold_start(&mut self) {
        self.clear_faults();
        self.checkpoint(true);
        self.verify("pre-cold-start");
        self.session = None;
        std::fs::remove_dir_all(&self.config.data_dir).expect("wipe data_dir");
        self.start_engine();
        self.verify("cold start");
        assert!(
            self.bucket.borrow().object_count() > 0,
            "seed {}: a cold start recovered from an empty bucket",
            self.seed
        );
    }

    /// Turns fault dice on for a stretch of ordinary work, then schedules
    /// the outage that ends in a crash — or just clears the storm.
    fn fault_storm(&mut self) {
        let transient = 20 + self.rng.next_bounded(120);
        let ambiguous = 20 + self.rng.next_bounded(120);
        let rot = self.rng.next_bounded(80);
        {
            let mut bucket = self.faults();
            bucket.faults.transient_per_mille = transient;
            bucket.faults.ambiguous_put_per_mille = ambiguous;
            bucket.faults.rot_per_mille = rot;
        }
        let bursts = 1 + self.rng.next_bounded(3);
        for _ in 0..bursts {
            self.dml_burst();
        }
        if self.rng.next_bounded(2) == 0 {
            // The outage begins somewhere in the near future and never
            // lifts: from the engine's view, the bucket dies mid-sequence.
            let ahead = u64::from(self.rng.next_bounded(60));
            {
                let mut bucket = self.faults();
                let at = bucket.op_count + ahead;
                bucket.faults.fail_from_op = Some(at);
            }
            self.dml_burst();
            self.checkpoint(false);
            self.crash_and_restart("outage crash");
        } else {
            self.clear_faults();
        }
    }

    fn step(&mut self) {
        self.steps_taken += 1;
        match self.rng.next_bounded(100) {
            0..55 => self.dml_burst(),
            55..65 => self.checkpoint(false),
            65..80 => self.fault_storm(),
            80..90 => self.crash_and_restart("warm restart"),
            90..95 => self.cold_start(),
            _ => self.verify("periodic"),
        }
    }
}

impl Drop for World {
    fn drop(&mut self) {
        self.session = None;
        drop_bucket(&self.config.s3_bucket);
        let _ = std::fs::remove_dir_all(&self.config.data_dir);
    }
}

fn apply(model: &mut BTreeMap<i64, Row>, id: i64, state: Option<Row>) {
    match state {
        Some(row) => {
            model.insert(id, row);
        }
        None => {
            model.remove(&id);
        }
    }
}

/// Reads a simple-protocol response: DataRows as `(id, Row)` and the first
/// ErrorResponse's (SQLSTATE, message).
fn parse_outcome(mut bytes: &[u8]) -> Outcome {
    let mut outcome = Outcome { rows: Vec::new(), affected: None, error: None };
    while bytes.len() >= 5 {
        let kind = bytes[0];
        let len = i32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize;
        let body = &bytes[5..1 + len];
        match kind {
            b'D' => {
                let mut fields = Vec::new();
                let n = u16::from_be_bytes(body[0..2].try_into().unwrap()) as usize;
                let mut at = 2;
                for _ in 0..n {
                    let flen = i32::from_be_bytes(body[at..at + 4].try_into().unwrap());
                    at += 4;
                    if flen < 0 {
                        fields.push(None);
                    } else {
                        let end = at + flen as usize;
                        fields.push(Some(
                            String::from_utf8(body[at..end].to_vec()).expect("text output"),
                        ));
                        at = end;
                    }
                }
                assert_eq!(fields.len(), 3, "the verification query has 3 columns");
                let id: i64 = fields[0].as_deref().unwrap().parse().unwrap();
                let v: i64 = fields[1].as_deref().unwrap().parse().unwrap();
                let pad = fields[2].clone().unwrap();
                outcome.rows.push((id, Row { v, pad }));
            }
            b'C' => {
                // "INSERT 0 1" / "UPDATE 0" / "DELETE 1": the trailing
                // number is the row count.
                let tag = std::str::from_utf8(&body[..body.len().saturating_sub(1)]).unwrap();
                outcome.affected = tag
                    .rsplit(' ')
                    .next()
                    .and_then(|n| n.parse().ok());
            }
            b'E' if outcome.error.is_none() => {
                let mut code = String::new();
                let mut message = String::new();
                let mut at = 0;
                while at < body.len() && body[at] != 0 {
                    let tag = body[at];
                    let end = at + 1
                        + body[at + 1..]
                            .iter()
                            .position(|&b| b == 0)
                            .expect("field terminator");
                    let text = std::str::from_utf8(&body[at + 1..end]).unwrap();
                    if tag == b'C' {
                        code = text.to_string();
                    } else if tag == b'M' {
                        message = text.to_string();
                    }
                    at = end + 1;
                }
                outcome.error = Some((code, message));
            }
            _ => {}
        }
        bytes = &bytes[1 + len..];
    }
    outcome
}

fn env_or(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[test]
fn storage_vopr() {
    let seed0 = env_or("POS3QL_STORAGE_VOPR_SEED0", 0x705e3);
    let seeds = env_or("POS3QL_STORAGE_VOPR_SEEDS", 4);
    let steps = env_or("POS3QL_STORAGE_VOPR_STEPS", 120);
    for seed in seed0..seed0 + seeds {
        let mut world = World::new(seed);
        for _ in 0..steps {
            world.step();
        }
        world.cold_start();
        println!(
            "storage vopr seed {seed}: {} steps, {} rows live, {} objects in the bucket",
            world.steps_taken,
            world.model.len(),
            world.bucket.borrow().object_count(),
        );
    }
}
