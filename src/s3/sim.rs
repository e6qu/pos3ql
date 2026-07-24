//! The virtual bucket: a deterministic, in-process object store standing in
//! for S3 behind [`super::ObjectClient`] — the storage VOPR's seam.
//!
//! `s3 = sim` routes every object operation here instead of a socket. The
//! bucket is plain memory shared by every client opened on the same name
//! (the checkpointer holds two, exactly as it holds two real clients), and
//! every fault it injects is drawn from a PCG stream, so a failing run
//! reproduces exactly from its seed. The faults are the ones a real bucket
//! serves up: transient request failures, the ambiguous PUT that landed but
//! whose response was lost, a flipped bit on the wire, and an outage that
//! starts mid-operation-sequence and never ends — the simulator's stand-in
//! for a crash, since everything after it is what a restarted process would
//! find.
//!
//! The bucket also *watches*: an unconditional overwrite that changes an
//! existing object's bytes is recorded, because the engine's key discipline
//! forbids it — blocks are content-addressed (a rewrite is byte-identical
//! by construction), the manifest moves only by compare-and-swap, and a WAL
//! segment may only *grow*: its key is the batch's first LSN, and when an
//! upload fails the batch marker is retained, so the retry carries the old
//! bytes plus newly committed ones — the old object must be a prefix of
//! the new. A recorded blind overwrite is a failed invariant, not a logged
//! curiosity.
//!
//! This module allocates freely (a growing map of objects is the point);
//! it exists for simulation tests, and `main` refuses `s3 = sim`.

use std::cell::RefCell;
use std::rc::Rc;

use crate::config::Config;
use crate::mem::budget::Budget;
use crate::mem::buffer::FixedBuf;
use crate::prng::Pcg32;
use crate::stack_format;
use crate::util::StackStr;

use super::{GetResult, Precondition, S3Error, S3SetupError};

/// Fault probabilities in parts per thousand, plus the outage schedule.
/// All zeros (the default) is a perfectly healthy bucket.
#[derive(Debug, Clone, Default)]
pub(crate) struct FaultPlan {
    /// Every operation whose index is at or past this fails with an I/O
    /// error: an outage, and — because nothing after it succeeds — the
    /// simulator's crash point.
    pub(crate) fail_from_op: Option<u64>,
    /// A request that fails cleanly: nothing applied.
    pub(crate) transient_per_mille: u32,
    /// A PUT that applies but whose response is lost — the caller sees an
    /// I/O error and cannot know the object landed.
    pub(crate) ambiguous_put_per_mille: u32,
    /// One bit of a GET body flipped in flight (the stored object stays
    /// intact). A reader that believes the bytes has no integrity checking.
    pub(crate) rot_per_mille: u32,
}

struct StoredObject {
    key: String,
    bytes: Vec<u8>,
    etag: u64,
}

/// The shared bucket state. One per name, held behind `Rc<RefCell<..>>` by
/// every [`SimClient`] opened on it and by the test harness steering faults.
pub(crate) struct SimBucket {
    /// Sorted by key, so LIST order is S3's lexicographic order.
    objects: Vec<StoredObject>,
    next_etag: u64,
    /// Operations served so far — the clock `fail_from_op` is measured on.
    pub(crate) op_count: u64,
    pub(crate) faults: FaultPlan,
    rng: Pcg32,
    /// Keys whose bytes an unconditional PUT changed — see the module doc.
    pub(crate) blind_overwrites: Vec<String>,
}

impl SimBucket {
    fn new(seed: u64) -> Self {
        Self {
            objects: Vec::new(),
            next_etag: 1,
            op_count: 0,
            faults: FaultPlan::default(),
            rng: Pcg32::new(seed, 0x0b1e_c757), // object-store stream
            blind_overwrites: Vec::new(),
        }
    }

    /// Test-only observability: the simulator asserts a cold start had
    /// something to come back from.
    #[cfg(test)]
    pub(crate) fn object_count(&self) -> usize {
        self.objects.len()
    }

    fn find(&self, key: &str) -> Result<usize, usize> {
        self.objects.binary_search_by(|o| o.key.as_str().cmp(key))
    }

    /// Rolls one fault die. `per_mille` of 0 never fires.
    fn roll(&mut self, per_mille: u32) -> bool {
        per_mille > 0 && self.rng.next_bounded(1000) < per_mille
    }

    /// The per-operation gate: counts the operation and reports whether the
    /// outage window has swallowed it.
    fn operation_gate(&mut self) -> Result<(), S3Error> {
        let index = self.op_count;
        self.op_count += 1;
        if self.faults.fail_from_op.is_some_and(|from| index >= from) {
            return Err(io_fault("simulated outage"));
        }
        if self.roll(self.faults.transient_per_mille) {
            return Err(io_fault("simulated transient failure"));
        }
        Ok(())
    }
}

fn io_fault(detail: &str) -> S3Error {
    S3Error::Io {
        context: "virtual bucket",
        kind: std::io::ErrorKind::ConnectionReset,
        detail: stack_format!(160, "{detail}"),
    }
}

fn etag_text(etag: u64) -> StackStr<80> {
    stack_format!(80, "\"sim-{etag:016x}\"")
}

thread_local! {
    /// One bucket per name per thread. Tests run on their own threads and
    /// name buckets uniquely, so incarnations of the same engine (restart,
    /// cold start) find the same bucket while tests stay isolated.
    static BUCKETS: RefCell<Vec<(String, Rc<RefCell<SimBucket>>)>> =
        const { RefCell::new(Vec::new()) };
}

/// Opens (or creates) the named bucket. The harness opens it first to hold
/// the fault-steering handle; the engine's clients then share it.
pub(crate) fn open_bucket(name: &str, seed: u64) -> Rc<RefCell<SimBucket>> {
    BUCKETS.with(|buckets| {
        let mut buckets = buckets.borrow_mut();
        if let Some((_, bucket)) = buckets.iter().find(|(n, _)| n == name) {
            return Rc::clone(bucket);
        }
        let bucket = Rc::new(RefCell::new(SimBucket::new(seed)));
        buckets.push((name.to_string(), Rc::clone(&bucket)));
        bucket
    })
}

/// Drops the named bucket, so a harness can start a world from nothing.
#[cfg(test)]
pub(crate) fn drop_bucket(name: &str) {
    BUCKETS.with(|buckets| buckets.borrow_mut().retain(|(n, _)| n != name));
}

/// The client half: what [`super::ObjectClient::Sim`] holds. Mirrors the
/// real client's observable semantics — the fixed response buffer (and its
/// `ResponseTooLarge`), inclusive ranges with 416 past the end, 404/412
/// statuses, DELETE of a missing key succeeding, LIST in key order with the
/// configured key prefix stripped.
pub(crate) struct SimClient {
    bucket: Rc<RefCell<SimBucket>>,
    key_prefix: String,
    body: FixedBuf,
}

impl SimClient {
    pub(crate) fn new(config: &Config, budget: &mut Budget) -> Result<Self, S3SetupError> {
        Ok(Self {
            bucket: open_bucket(&config.s3_bucket, 0),
            key_prefix: config.s3_prefix.clone(),
            body: FixedBuf::new(budget, "s3_response", config.s3_response_bytes)?,
        })
    }

    fn full_key(&self, key: &str) -> String {
        let mut full = String::with_capacity(self.key_prefix.len() + key.len());
        full.push_str(&self.key_prefix);
        full.push_str(key);
        full
    }

    pub(crate) fn put(
        &mut self,
        key: &str,
        body: &[u8],
        precondition: Precondition,
    ) -> Result<StackStr<80>, S3Error> {
        let full = self.full_key(key);
        let mut bucket = self.bucket.borrow_mut();
        bucket.operation_gate()?;
        let position = bucket.find(&full);
        match (&precondition, &position) {
            (Precondition::IfNoneMatchAny, Ok(_)) => {
                return Err(status(412, "precondition failed: object exists"));
            }
            (Precondition::IfMatch(expected), Ok(at)) => {
                if etag_text(bucket.objects[*at].etag).as_str() != *expected {
                    return Err(status(412, "precondition failed: etag mismatch"));
                }
            }
            (Precondition::IfMatch(_), Err(_)) => {
                return Err(status(412, "precondition failed: no such object"));
            }
            _ => {}
        }
        if let (Precondition::None, Ok(at)) = (&precondition, &position) {
            let old = bucket.objects[*at].bytes.as_slice();
            let logical = &full[self.key_prefix.len()..];
            let segment_growth =
                logical.starts_with("wal/") && body.len() > old.len() && body.starts_with(old);
            if old != body && !segment_growth {
                let key = full.clone();
                bucket.blind_overwrites.push(key);
            }
        }
        let etag = bucket.next_etag;
        bucket.next_etag += 1;
        match position {
            Ok(at) => {
                bucket.objects[at].bytes = body.to_vec();
                bucket.objects[at].etag = etag;
            }
            Err(at) => bucket.objects.insert(
                at,
                StoredObject { key: full, bytes: body.to_vec(), etag },
            ),
        }
        let ambiguous_per_mille = bucket.faults.ambiguous_put_per_mille;
        if bucket.roll(ambiguous_per_mille) {
            // Applied above — the response is what got lost.
            return Err(io_fault("simulated ambiguous PUT (applied, response lost)"));
        }
        Ok(etag_text(etag))
    }

    pub(crate) fn get(&mut self, key: &str, range: Option<(u64, u64)>) -> Result<GetResult, S3Error> {
        let full = self.full_key(key);
        let mut bucket = self.bucket.borrow_mut();
        bucket.operation_gate()?;
        let at = match bucket.find(&full) {
            Ok(at) => at,
            Err(_) => return Err(status(404, "no such object")),
        };
        let total = bucket.objects[at].bytes.len();
        let (from, to) = match range {
            None => (0usize, total),
            Some((offset, to)) => {
                if offset >= total as u64 {
                    return Err(status(416, "range not satisfiable"));
                }
                (offset as usize, (to as usize + 1).min(total))
            }
        };
        let len = to - from;
        if len > self.body.capacity() {
            return Err(S3Error::ResponseTooLarge {
                content_length: len,
                capacity: self.body.capacity(),
            });
        }
        self.body.clear();
        assert!(
            self.body.append(&bucket.objects[at].bytes[from..to]),
            "capacity checked above"
        );
        let rot_per_mille = bucket.faults.rot_per_mille;
        if bucket.roll(rot_per_mille) && len > 0 {
            let bit = bucket.rng.next_bounded((len * 8) as u32) as usize;
            self.body.filled_mut()[bit / 8] ^= 1 << (bit % 8);
        }
        Ok(GetResult { len, etag: etag_text(bucket.objects[at].etag) })
    }

    pub(crate) fn body_bytes(&self) -> &[u8] {
        self.body.readable()
    }

    pub(crate) fn response_capacity(&self) -> usize {
        self.body.capacity()
    }

    pub(crate) fn delete(&mut self, key: &str) -> Result<(), S3Error> {
        let full = self.full_key(key);
        let mut bucket = self.bucket.borrow_mut();
        bucket.operation_gate()?;
        if let Ok(at) = bucket.find(&full) {
            bucket.objects.remove(at);
        }
        Ok(())
    }

    pub(crate) fn list(
        &mut self,
        prefix: &str,
        mut each: impl FnMut(&str),
    ) -> Result<usize, S3Error> {
        let full_prefix = self.full_key(prefix);
        let mut bucket = self.bucket.borrow_mut();
        bucket.operation_gate()?;
        let mut count = 0usize;
        for object in &bucket.objects {
            if object.key.starts_with(&full_prefix) {
                each(&object.key[self.key_prefix.len()..]);
                count += 1;
            }
        }
        Ok(count)
    }
}

fn status(code: u16, message: &str) -> S3Error {
    S3Error::Status { code, message: stack_format!(256, "{message}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(name: &str) -> (SimClient, Rc<RefCell<SimBucket>>) {
        drop_bucket(name);
        let bucket = open_bucket(name, 7);
        let mut config = Config::default_dev();
        config.s3_bucket = name.to_string();
        config.s3_prefix = "p/".to_string();
        config.s3_response_bytes = 64;
        let mut budget = Budget::new(1 << 20);
        (SimClient::new(&config, &mut budget).unwrap(), bucket)
    }

    #[test]
    fn round_trip_ranges_and_statuses() {
        let (mut c, _) = client("sim-rt");
        assert!(c.get("k", None).unwrap_err().is_not_found());
        let tag = c.put("k", b"hello world", Precondition::None).unwrap();
        let got = c.get("k", None).unwrap();
        assert_eq!(c.body_bytes(), b"hello world");
        assert_eq!(got.etag.as_str(), tag.as_str());
        // Inclusive range; a range past the end clamps; one starting past
        // the end is 416 (the WAL replay loop's terminator).
        c.get("k", Some((6, 100))).unwrap();
        assert_eq!(c.body_bytes(), b"world");
        assert!(matches!(
            c.get("k", Some((11, 12))).unwrap_err(),
            S3Error::Status { code: 416, .. }
        ));
        // Oversized bodies refuse like the real client.
        let big = [0u8; 65];
        c.put("big", &big, Precondition::None).unwrap();
        assert!(matches!(
            c.get("big", None).unwrap_err(),
            S3Error::ResponseTooLarge { .. }
        ));
        c.delete("missing").unwrap();
    }

    #[test]
    fn compare_and_swap_semantics() {
        let (mut c, _) = client("sim-cas");
        let first = c.put("m", b"v1", Precondition::IfNoneMatchAny).unwrap();
        assert!(c
            .put("m", b"v2", Precondition::IfNoneMatchAny)
            .unwrap_err()
            .is_precondition_failed());
        let second = c.put("m", b"v2", Precondition::IfMatch(first.as_str())).unwrap();
        // The stale tag loses.
        assert!(c
            .put("m", b"v3", Precondition::IfMatch(first.as_str()))
            .unwrap_err()
            .is_precondition_failed());
        c.put("m", b"v3", Precondition::IfMatch(second.as_str())).unwrap();
        assert!(c
            .put("absent", b"x", Precondition::IfMatch(second.as_str()))
            .unwrap_err()
            .is_precondition_failed());
    }

    #[test]
    fn blind_overwrite_is_recorded_but_identical_rewrite_is_not() {
        let (mut c, bucket) = client("sim-blind");
        c.put("b", b"same", Precondition::None).unwrap();
        c.put("b", b"same", Precondition::None).unwrap();
        assert!(bucket.borrow().blind_overwrites.is_empty());
        c.put("b", b"different", Precondition::None).unwrap();
        assert_eq!(bucket.borrow().blind_overwrites, vec!["p/b".to_string()]);
    }

    #[test]
    fn faults_fire_deterministically() {
        let (mut c, bucket) = client("sim-faults");
        c.put("k", b"payload", Precondition::None).unwrap();
        // Outage-from-op: everything past the mark fails.
        let mark = bucket.borrow().op_count;
        bucket.borrow_mut().faults.fail_from_op = Some(mark);
        assert!(matches!(c.get("k", None).unwrap_err(), S3Error::Io { .. }));
        assert!(matches!(
            c.put("k2", b"x", Precondition::None).unwrap_err(),
            S3Error::Io { .. }
        ));
        bucket.borrow_mut().faults.fail_from_op = None;
        // Ambiguous PUT: reported failed, actually applied.
        bucket.borrow_mut().faults.ambiguous_put_per_mille = 1000;
        assert!(matches!(
            c.put("amb", b"landed", Precondition::None).unwrap_err(),
            S3Error::Io { .. }
        ));
        bucket.borrow_mut().faults.ambiguous_put_per_mille = 0;
        c.get("amb", None).unwrap();
        assert_eq!(c.body_bytes(), b"landed");
        // Rot flips exactly one bit of the copy; the object stays intact.
        bucket.borrow_mut().faults.rot_per_mille = 1000;
        c.get("k", None).unwrap();
        let rotted = c.body_bytes().to_vec();
        assert_ne!(rotted, b"payload");
        let diff: u32 = rotted
            .iter()
            .zip(b"payload".iter())
            .map(|(a, b)| (a ^ b).count_ones())
            .sum();
        assert_eq!(diff, 1);
        bucket.borrow_mut().faults.rot_per_mille = 0;
        c.get("k", None).unwrap();
        assert_eq!(c.body_bytes(), b"payload");
    }

    #[test]
    fn list_strips_the_key_prefix_in_order() {
        let (mut c, _) = client("sim-list");
        c.put("wal/2", b"b", Precondition::None).unwrap();
        c.put("wal/1", b"a", Precondition::None).unwrap();
        c.put("blocks/x", b"c", Precondition::None).unwrap();
        let mut seen = Vec::new();
        let n = c.list("wal/", |k| seen.push(k.to_string())).unwrap();
        assert_eq!(n, 2);
        assert_eq!(seen, vec!["wal/1".to_string(), "wal/2".to_string()]);
    }
}
