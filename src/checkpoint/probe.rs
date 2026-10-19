//! One startup-reserved writer probe shared by waiting operational requests.

use std::time::{Duration, Instant};

use super::{SQLSTATE_CAS, WRITER_FENCE_KEY, WriterToken, writer_fence_record};
use crate::config::Config;
use crate::mem::budget::Budget;
use crate::mem::fixed_vec::FixedVec;
use crate::object_store::{Client, Credentials, Error};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

const ATTEMPTS: u8 = 3;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ticket(core::num::NonZeroU64, usize);

#[allow(
    clippy::large_enum_variant,
    reason = "fixed startup state retains bounded errors without runtime allocation"
)]
enum State {
    Idle,
    Releasing,
    Queued { at: Instant, attempts: u8 },
    Active { deadline: Instant, attempts: u8 },
    Complete(Result<(), SqlError>),
}

pub(crate) struct WriterProbe {
    client: Client,
    writer: WriterToken,
    state: State,
    generation: u64,
    consumers: usize,
    owners: FixedVec<bool>,
    fenced: bool,
}

impl WriterProbe {
    pub(super) fn budget_bytes(config: &Config) -> usize {
        Client::budget_bytes(config)
            + config.operations_max_connections * core::mem::size_of::<bool>()
    }

    pub(super) fn new(
        config: &Config,
        budget: &mut Budget,
        writer: WriterToken,
    ) -> Result<Self, super::CheckpointSetupError> {
        let mut client = Client::new(config, budget)
            .map_err(|error| super::CheckpointSetupError::ObjectStore(error.to_string()))?;
        client.enable_async_gets();
        let mut owners = FixedVec::new(
            budget,
            "writer probe owners",
            config.operations_max_connections,
        )
        .map_err(super::CheckpointSetupError::Budget)?;
        for _ in 0..config.operations_max_connections {
            owners.push(false).expect("startup owner capacity");
        }
        Ok(Self {
            client,
            writer,
            state: State::Idle,
            generation: 0,
            consumers: 0,
            owners,
            fenced: false,
        })
    }

    /// Completed results remain owned until their original consumers take them.
    /// New arrivals wait for a fresh probe rather than accepting cached readiness.
    pub(crate) fn request(&mut self, owner: usize) -> Result<Option<Ticket>, SqlError> {
        if owner >= self.owners.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "writer probe owner exceeds operations_max_connections ({})",
                self.owners.len()
            ));
        }
        if self.owners[owner] {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "writer probe owner already retains a request"
            ));
        }
        if matches!(self.state, State::Complete(_) | State::Releasing) {
            return Ok(None);
        }
        if matches!(self.state, State::Idle) {
            let generation = self
                .generation
                .checked_add(1)
                .and_then(core::num::NonZeroU64::new)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "writer probe identities exhausted"
                    )
                })?;
            self.generation = generation.get();
            self.state = State::Queued {
                at: Instant::now(),
                attempts: 0,
            };
        }
        self.consumers += 1;
        self.owners[owner] = true;
        Ok(Some(Ticket(
            core::num::NonZeroU64::new(self.generation)
                .expect("a live probe has a nonzero identity"),
            owner,
        )))
    }

    pub(crate) fn take(&mut self, ticket: Ticket) -> Option<Result<(), SqlError>> {
        self.validate(ticket);
        let State::Complete(result) = &self.state else {
            return None;
        };
        let result = match result {
            Ok(()) => Ok(()),
            Err(error) => Err(SqlError {
                sqlstate: error.sqlstate,
                message: error.message,
            }),
        };
        self.release(ticket.1);
        Some(result)
    }

    pub(crate) fn cancel(&mut self, ticket: Ticket) {
        self.validate(ticket);
        self.release(ticket.1);
    }

    fn validate(&self, ticket: Ticket) {
        assert!(
            ticket.0.get() == self.generation
                && self.owners.get(ticket.1).copied() == Some(true)
                && self.consumers != 0,
            "writer probe receipt belongs to its retained request"
        );
    }

    fn release(&mut self, owner: usize) {
        self.owners[owner] = false;
        self.consumers -= 1;
        if self.consumers == 0 {
            // Keep a live descriptor owned until the reactor's control turn,
            // before another client can reuse its integer descriptor.
            self.state = State::Releasing;
        }
    }

    pub(crate) fn advance(&mut self, now: Instant) {
        if matches!(self.state, State::Releasing) {
            self.client.cancel_request();
            self.state = State::Idle;
            return;
        }
        if matches!(self.state, State::Idle | State::Complete(_)) {
            return;
        }
        if self.fenced {
            self.client.cancel_request();
            self.state = State::Complete(Err(sql_err!(
                SQLSTATE_CAS,
                "writer was fenced by a newer process"
            )));
            return;
        }
        let (attempts, result) = match self.state {
            State::Queued { at, attempts } if at <= now => {
                self.state = State::Active {
                    deadline: now + REQUEST_TIMEOUT,
                    attempts: attempts + 1,
                };
                (attempts + 1, self.client.get(WRITER_FENCE_KEY, None))
            }
            State::Active { deadline, attempts } => {
                if deadline <= now {
                    (
                        attempts,
                        Err(Error::Io {
                            context: "writer probe deadline",
                            kind: std::io::ErrorKind::TimedOut,
                            detail: crate::util::StackStr::new(),
                        }),
                    )
                } else {
                    (attempts, self.client.advance_get())
                }
            }
            _ => return,
        };
        match result {
            Ok(_) => {
                let expected = writer_fence_record("active", self.writer);
                if self.client.body_bytes() == expected.as_str().as_bytes() {
                    self.state = State::Complete(Ok(()));
                } else {
                    self.fenced = true;
                    self.state = State::Complete(Err(sql_err!(
                        SQLSTATE_CAS,
                        "writer was fenced by a newer process"
                    )));
                }
            }
            Err(Error::WouldBlock) => {}
            Err(error) => {
                self.client.cancel_request();
                if error.is_retryable() && attempts < ATTEMPTS {
                    self.state = State::Queued {
                        at: now + Duration::from_millis(100 << attempts),
                        attempts,
                    };
                } else {
                    self.state = State::Complete(Err(sql_err!(
                        SQLSTATE_CAS,
                        "verify writer ownership: {}",
                        error
                    )));
                }
            }
        }
    }

    pub(crate) fn busy(&self) -> bool {
        matches!(
            self.state,
            State::Queued { .. } | State::Active { .. } | State::Releasing
        )
    }

    pub(crate) fn fenced(&self) -> bool {
        self.fenced
    }

    pub(super) fn fence(&mut self) {
        self.fenced = true;
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        match self.state {
            State::Releasing => Some(Instant::now()),
            State::Queued { at, .. } => Some(at),
            State::Active { deadline, .. } => Some(deadline),
            _ => None,
        }
    }

    pub(crate) fn fd(&self) -> Option<std::os::fd::RawFd> {
        self.client.pending_fd()
    }

    pub(crate) fn wants_write(&self) -> bool {
        self.client.pending_wants_write()
    }

    pub(super) fn replace_credentials(&mut self, credentials: Credentials) {
        assert!(
            !self.busy(),
            "credential replacement waits for the active writer probe"
        );
        self.client.replace_credentials(credentials);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::{Precondition, sim};

    fn simulated(label: &str) -> (WriterProbe, sim::SimNamespaceHandle, Config) {
        let mut config = Config::default_dev();
        config.object_store_on = true;
        config.object_store_sim = true;
        config.object_store_bucket = format!("{label}-{}", std::process::id());
        config.operations_max_connections = 2;
        sim::drop_namespace(&config.object_store_bucket);
        let namespace = sim::open_namespace(&config.object_store_bucket, 281);
        let mut budget = Budget::new(WriterProbe::budget_bytes(&config));
        let mut probe = WriterProbe::new(&config, &mut budget, WriterToken(1)).unwrap();
        let record = writer_fence_record("active", WriterToken(1));
        probe
            .client
            .put(
                WRITER_FENCE_KEY,
                record.as_str().as_bytes(),
                Precondition::IfNoneMatchAny,
            )
            .unwrap();
        (probe, namespace, config)
    }

    #[test]
    fn parallel_writer_probe_retains_each_result_and_requires_a_fresh_request() {
        let (mut probe, namespace, config) = simulated("writer-probe-owners");
        crate::mem::guard::forbid_alloc(|| {
            let first = probe.request(0).unwrap().unwrap();
            let second = probe.request(1).unwrap().unwrap();
            assert_eq!(first.0, second.0);
            assert_ne!(first, second);
            assert_eq!(
                probe.request(2).unwrap_err().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert!(probe.take(first).is_none());
            probe.advance(Instant::now());
            probe.take(first).unwrap().unwrap();
            assert!(
                probe.request(0).unwrap().is_none(),
                "a new caller cannot consume a completed earlier probe"
            );
            probe.take(second).unwrap().unwrap();
            probe.advance(Instant::now());
            let fresh = probe.request(0).unwrap().unwrap();
            assert_ne!(fresh, first);
            let operations = namespace.borrow().op_count;
            probe.cancel(fresh);
            assert!(probe.busy());
            probe.advance(Instant::now());
            assert!(!probe.busy() && probe.deadline().is_none());
            assert_eq!(
                namespace.borrow().op_count,
                operations,
                "cancel before dispatch performs no request"
            );
        });
        drop(probe);
        sim::drop_namespace(&config.object_store_bucket);
    }

    #[test]
    fn parallel_writer_probe_retries_transport_errors_and_fences_changed_owners() {
        let (mut probe, namespace, config) = simulated("writer-probe-errors");
        let first = probe.request(0).unwrap().unwrap();
        let second = probe.request(1).unwrap().unwrap();
        probe.cancel(first);
        namespace.borrow_mut().faults.transient_per_mille = 1000;
        crate::mem::guard::forbid_alloc(|| {
            let now = Instant::now();
            for turn in 0..16 {
                probe.advance(now + Duration::from_millis(turn * 250));
                if matches!(probe.state, State::Complete(_)) {
                    break;
                }
            }
            assert_eq!(
                probe.take(second).unwrap().unwrap_err().sqlstate,
                SQLSTATE_CAS
            );
            assert!(
                !probe.fenced(),
                "transport failure cannot invent another owner"
            );
            probe.advance(Instant::now());
        });
        namespace.borrow_mut().faults.transient_per_mille = 0;
        let key = format!("{}{}", config.object_store_prefix, WRITER_FENCE_KEY);
        assert!(namespace.borrow().object_bytes(&key).is_some());
        let old = probe.client.get(WRITER_FENCE_KEY, None).unwrap().etag;
        let replaced = writer_fence_record("active", WriterToken(2));
        probe
            .client
            .put(
                WRITER_FENCE_KEY,
                replaced.as_str().as_bytes(),
                Precondition::IfMatch(&old),
            )
            .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let request = probe.request(0).unwrap().unwrap();
            probe.advance(Instant::now());
            assert_eq!(
                probe.take(request).unwrap().unwrap_err().sqlstate,
                SQLSTATE_CAS
            );
            assert!(probe.fenced());
            probe.advance(Instant::now());
            let request = probe.request(0).unwrap().unwrap();
            let operations = namespace.borrow().op_count;
            probe.advance(Instant::now());
            assert_eq!(
                probe.take(request).unwrap().unwrap_err().sqlstate,
                SQLSTATE_CAS
            );
            assert_eq!(
                namespace.borrow().op_count,
                operations,
                "a fenced writer cannot probe itself back into service"
            );
            probe.advance(Instant::now());
        });
        drop(probe);
        sim::drop_namespace(&config.object_store_bucket);
    }

    #[test]
    fn parallel_writer_probe_held_http_response_yields_and_releases_on_deadline() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::mpsc;
        for timeout in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut config =
                crate::object_store::upload::tests::config(listener.local_addr().unwrap().port());
            config.operations_max_connections = 2;
            let (ready, observed) = mpsc::channel();
            let (release, held) = mpsc::channel();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut head = Vec::new();
                let mut part = [0u8; 4096];
                while !head.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let n = stream.read(&mut part).unwrap();
                    assert_ne!(n, 0);
                    head.extend_from_slice(&part[..n]);
                }
                let head = core::str::from_utf8(&head).unwrap();
                assert!(head.starts_with("GET "));
                assert!(head.contains("/writer-fence HTTP/1.1\r\n"));
                assert!(head.contains("authorization: AWS4-HMAC-SHA256 "));
                ready.send(()).unwrap();
                held.recv_timeout(Duration::from_secs(5)).unwrap();
                let record = writer_fence_record("active", WriterToken(1));
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nETag: \"probe\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    record.len(),
                    record.as_str()
                );
            });
            let mut budget = Budget::new(WriterProbe::budget_bytes(&config));
            let mut probe = WriterProbe::new(&config, &mut budget, WriterToken(1)).unwrap();
            assert_eq!(budget.remaining(), 0);
            let ticket = probe.request(0).unwrap().unwrap();
            crate::mem::guard::forbid_alloc(|| {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    assert!(
                        Instant::now() < deadline,
                        "probe did not send its nonblocking GET"
                    );
                    probe.advance(Instant::now());
                    assert!(
                        probe.take(ticket).is_none(),
                        "an unfinished probe cannot report readiness"
                    );
                    match observed.try_recv() {
                        Ok(()) => break,
                        Err(mpsc::TryRecvError::Empty) => std::thread::yield_now(),
                        Err(error) => panic!("test server stopped: {error}"),
                    }
                }
                assert!(probe.fd().is_some() && probe.deadline().is_some());
                if timeout {
                    let expired = Instant::now() + Duration::from_secs(31);
                    probe.advance(expired);
                    assert!(probe.fd().is_none());
                    assert!(probe.take(ticket).is_none());
                    assert!(probe.deadline().is_some_and(|retry| retry > expired));
                    probe.cancel(ticket);
                    probe.advance(expired);
                    assert!(!probe.busy() && probe.deadline().is_none());
                }
            });
            release.send(()).unwrap();
            if !timeout {
                crate::mem::guard::forbid_alloc(|| {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    loop {
                        assert!(Instant::now() < deadline, "released probe did not complete");
                        probe.advance(Instant::now());
                        if let Some(result) = probe.take(ticket) {
                            result.unwrap();
                            break;
                        }
                        std::thread::yield_now();
                    }
                    probe.advance(Instant::now());
                    assert!(!probe.busy() && !probe.fenced());
                });
            }
            server.join().unwrap();
        }
    }
}
