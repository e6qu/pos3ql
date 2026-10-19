//! Startup-reserved request ownership for concurrent object uploads.

use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::mem::budget::Budget;
use crate::mem::buffer::FixedBuf;
use crate::mem::fixed_vec::FixedVec;
use crate::util::StackStr;

use super::{
    ByteRange, Client, EntityTag, Error, GetResult, MAX_OBJECT_KEY_BYTES, Precondition, SetupError,
};

const ATTEMPTS: u8 = 3;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ticket {
    slot: usize,
    generation: u64,
}

#[derive(Clone, Copy)]
pub(crate) enum Condition {
    ImmutableCreate,
    PublishAbsent,
    PublishMatch(EntityTag),
}

impl Condition {
    fn borrowed(&self) -> Precondition<'_> {
        match self {
            Self::ImmutableCreate | Self::PublishAbsent => Precondition::IfNoneMatchAny,
            Self::PublishMatch(etag) => Precondition::IfMatch(etag),
        }
    }
}

#[derive(Debug)]
pub(crate) enum UploadError {
    SlotsExhausted,
    BodyTooLarge { requested: usize, capacity: usize },
    KeyTooLarge,
    GenerationExhausted,
    InvalidTicket,
    Request(Error),
}

enum State {
    Free,
    Queued {
        retry_at: Instant,
    },
    Active {
        deadline: Instant,
    },
    VerifyQueued {
        offset: usize,
        etag: Option<EntityTag>,
    },
    VerifyActive {
        offset: usize,
        etag: Option<EntityTag>,
        deadline: Instant,
    },
    Complete(Result<EntityTag, Error>),
}

struct Slot {
    client: Client,
    body: FixedBuf,
    key: StackStr<MAX_OBJECT_KEY_BYTES>,
    condition: Condition,
    generation: u64,
    attempts: u8,
    conditional_failure: Option<Error>,
    state: State,
}

pub(crate) struct Uploads {
    slots: FixedVec<Slot>,
    pollfds: FixedVec<libc::pollfd>,
}

impl Uploads {
    pub(crate) fn budget_bytes(config: &Config) -> usize {
        config.object_store_put_slots.saturating_mul(
            size_of::<Slot>()
                + size_of::<libc::pollfd>()
                + Client::budget_bytes(config)
                + config.wal_upload_buffer_bytes,
        )
    }

    pub(crate) fn new(config: &Config, budget: &mut Budget) -> Result<Self, SetupError> {
        if config.object_store_put_slots < 2 {
            return Err(SetupError::Capacity(
                "object_store_put_slots must be at least 2",
            ));
        }
        if config.wal_upload_buffer_bytes == 0 {
            return Err(SetupError::Capacity(
                "wal_upload_buffer_bytes must be greater than zero",
            ));
        }
        let mut slots = FixedVec::new(
            budget,
            "object_store_put_slots",
            config.object_store_put_slots,
        )?;
        for _ in 0..config.object_store_put_slots {
            let slot = Slot {
                client: {
                    let mut client = Client::new(config, budget)?;
                    client.enable_async_gets();
                    client
                },
                body: FixedBuf::new(
                    budget,
                    "object_store_upload_body",
                    config.wal_upload_buffer_bytes,
                )?,
                key: StackStr::new(),
                condition: Condition::ImmutableCreate,
                generation: 0,
                attempts: 0,
                conditional_failure: None,
                state: State::Free,
            };
            slots
                .push(slot)
                .unwrap_or_else(|_| unreachable!("reserved upload slots"));
        }
        Ok(Self {
            slots,
            pollfds: FixedVec::new(
                budget,
                "object upload poll descriptors",
                config.object_store_put_slots,
            )?,
        })
    }

    /// Copies the bytes before starting I/O; the producer may then reuse its
    /// buffer. Completion belongs to this generation until its ticket is consumed.
    pub(crate) fn enqueue(
        &mut self,
        key: &str,
        body: &[u8],
        condition: Condition,
    ) -> Result<Ticket, UploadError> {
        if key.len() > MAX_OBJECT_KEY_BYTES {
            return Err(UploadError::KeyTooLarge);
        }
        let (index, slot) = self
            .slots
            .iter_mut()
            .enumerate()
            .find(|(_, slot)| matches!(slot.state, State::Free))
            .ok_or(UploadError::SlotsExhausted)?;
        if body.len() > slot.body.capacity() {
            return Err(UploadError::BodyTooLarge {
                requested: body.len(),
                capacity: slot.body.capacity(),
            });
        }
        let generation = slot
            .generation
            .checked_add(1)
            .ok_or(UploadError::GenerationExhausted)?;
        slot.body.clear();
        assert!(slot.body.append(body));
        slot.key = StackStr::from_str(key);
        slot.condition = condition;
        slot.generation = generation;
        slot.attempts = 0;
        slot.conditional_failure = None;
        slot.state = State::Queued {
            retry_at: Instant::now(),
        };
        Ok(Ticket {
            slot: index,
            generation,
        })
    }

    /// Every slot gets one bounded transport turn. A ready slot cannot be
    /// recycled by another producer before its consumer takes the outcome.
    pub(crate) fn advance(&mut self, now: Instant) {
        for slot in self.slots.iter_mut() {
            match slot.state {
                State::VerifyQueued { offset, etag } => {
                    slot.state = State::VerifyActive {
                        offset,
                        etag,
                        deadline: now + REQUEST_TIMEOUT,
                    };
                    let capacity = slot.client.response_capacity();
                    let range = if slot.body.len() <= capacity {
                        None
                    } else {
                        let remaining = slot.body.len() - offset;
                        let want = capacity.min(remaining.saturating_add(1));
                        Some(
                            ByteRange::new(offset as u64, (offset + want - 1) as u64)
                                .expect("nonempty bounded verification range"),
                        )
                    };
                    let result = slot.client.get(slot.key.as_str(), range);
                    Self::verified(slot, offset, etag, result, now);
                }
                State::VerifyActive {
                    offset,
                    etag,
                    deadline,
                } => {
                    let result = if deadline <= now {
                        Err(Self::timeout())
                    } else {
                        slot.client.advance_get()
                    };
                    Self::verified(slot, offset, etag, result, now);
                }
                _ => {
                    let result = match slot.state {
                        State::Queued { retry_at } if retry_at <= now => {
                            slot.attempts += 1;
                            slot.state = State::Active {
                                deadline: now + REQUEST_TIMEOUT,
                            };
                            slot.client.begin_put(
                                slot.key.as_str(),
                                slot.body.readable(),
                                slot.condition.borrowed(),
                            )
                        }
                        State::Active { deadline } if deadline <= now => Err(Self::timeout()),
                        State::Active { .. } => slot.client.advance_put(slot.body.readable()),
                        _ => continue,
                    };
                    match result {
                        Ok(etag) => slot.state = State::Complete(Ok(etag)),
                        Err(Error::WouldBlock) => {}
                        Err(error) if error.is_precondition_failed() => {
                            slot.conditional_failure = Some(error);
                            // A lost create response can leave the object durable.
                            // Compare every byte, including length, before accepting it.
                            slot.state = State::VerifyQueued {
                                offset: 0,
                                etag: None,
                            };
                        }
                        Err(error) => Self::failed(slot, error, now),
                    }
                }
            }
        }
    }

    fn timeout() -> Error {
        Error::Io {
            context: "object upload deadline",
            kind: std::io::ErrorKind::TimedOut,
            detail: StackStr::new(),
        }
    }

    fn failed(slot: &mut Slot, error: Error, now: Instant) {
        slot.client.cancel_request();
        if error.is_retryable() && slot.attempts < ATTEMPTS {
            slot.state = State::Queued {
                retry_at: now + Duration::from_millis(100 << slot.attempts),
            };
        } else {
            slot.state = State::Complete(Err(error));
        }
    }

    fn comparison_failure(slot: &mut Slot) -> Error {
        match slot.condition {
            Condition::ImmutableCreate => Error::Protocol("immutable object collision"),
            Condition::PublishAbsent | Condition::PublishMatch(_) => slot
                .conditional_failure
                .take()
                .expect("publication comparison follows a failed conditional request"),
        }
    }

    fn verified(
        slot: &mut Slot,
        offset: usize,
        prior_etag: Option<EntityTag>,
        result: Result<GetResult, Error>,
        now: Instant,
    ) {
        let result = match result {
            Err(Error::WouldBlock) => return,
            Err(Error::Status { code: 416, .. })
                if offset == slot.body.len() && prior_etag.is_some() =>
            {
                slot.state = State::Complete(Ok(prior_etag.expect("checked verified generation")));
                return;
            }
            Err(Error::Status { code: 416, .. }) => {
                let error = Self::comparison_failure(slot);
                Self::failed(slot, error, now);
                return;
            }
            Err(error) => {
                Self::failed(slot, error, now);
                return;
            }
            Ok(result) => result,
        };
        let remaining = slot.body.len() - offset;
        let capacity = slot.client.response_capacity();
        let expected = remaining.min(capacity);
        if (offset == slot.body.len() && slot.body.len() > capacity)
            || result.len != expected
            || prior_etag.is_some_and(|etag| etag != result.etag)
            || slot.client.body_bytes() != &slot.body.readable()[offset..offset + expected]
        {
            let error = Self::comparison_failure(slot);
            Self::failed(slot, error, now);
            return;
        }
        let end = offset + expected;
        // Whole GET checks the full length. A final short range asks for one
        // extra byte; an exact-capacity ending needs a separate 416 probe.
        if slot.body.len() <= capacity || (end == slot.body.len() && expected < capacity) {
            slot.state = State::Complete(Ok(result.etag));
        } else {
            slot.state = State::VerifyQueued {
                offset: end,
                etag: Some(result.etag),
            };
        }
    }

    /// Explicit synchronous completion for startup and manual maintenance.
    /// Ordinary server progress uses advance and reactor readiness instead.
    pub(crate) fn finish(&mut self, ticket: Ticket) -> Result<EntityTag, UploadError> {
        loop {
            self.advance(Instant::now());
            if let Some(etag) = self.take(ticket)? {
                return Ok(etag);
            }
            if let Err(error) = self.wait_ready() {
                self.cancel(ticket)?;
                return Err(UploadError::Request(error));
            }
        }
    }

    pub(crate) fn wait_ready(&mut self) -> Result<(), Error> {
        self.pollfds.clear();
        for slot in self.slots.iter() {
            if let Some(fd) = slot.client.pending_fd() {
                self.pollfds
                    .push(libc::pollfd {
                        fd,
                        events: libc::POLLIN
                            | if slot.client.pending_wants_write() {
                                libc::POLLOUT
                            } else {
                                0
                            },
                        revents: 0,
                    })
                    .expect("one descriptor per upload slot");
            }
        }
        let deadline = self
            .next_deadline()
            .ok_or(Error::Protocol("no pending upload to wait for"))?;
        let timeout = deadline
            .saturating_duration_since(Instant::now())
            .as_nanos()
            .div_ceil(1_000_000)
            .min(i32::MAX as u128) as i32;
        let result = unsafe {
            libc::poll(
                self.pollfds.as_mut_slice().as_mut_ptr(),
                self.pollfds.len() as libc::nfds_t,
                timeout,
            )
        };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(Error::Io {
                    context: "object upload readiness",
                    kind: error.kind(),
                    detail: StackStr::new(),
                });
            }
        }
        Ok(())
    }

    fn slot_mut(&mut self, ticket: Ticket) -> Result<&mut Slot, UploadError> {
        let slot = self
            .slots
            .get_mut(ticket.slot)
            .ok_or(UploadError::InvalidTicket)?;
        if slot.generation != ticket.generation || matches!(slot.state, State::Free) {
            return Err(UploadError::InvalidTicket);
        }
        Ok(slot)
    }

    pub(crate) fn take(&mut self, ticket: Ticket) -> Result<Option<EntityTag>, UploadError> {
        let slot = self.slot_mut(ticket)?;
        if !matches!(slot.state, State::Complete(_)) {
            return Ok(None);
        }
        let State::Complete(result) = core::mem::replace(&mut slot.state, State::Free) else {
            unreachable!("checked completed request");
        };
        slot.body.clear();
        slot.key.clear();
        slot.conditional_failure = None;
        result.map(Some).map_err(UploadError::Request)
    }

    /// Cancellation says nothing about whether an already sent PUT landed.
    /// Publication owners must reconcile an uncertain conditional root separately.
    pub(crate) fn cancel(&mut self, ticket: Ticket) -> Result<(), UploadError> {
        let slot = self.slot_mut(ticket)?;
        slot.client.cancel_request();
        slot.state = State::Free;
        slot.body.clear();
        slot.key.clear();
        slot.conditional_failure = None;
        Ok(())
    }

    pub(crate) fn slots(&self) -> usize {
        self.slots.len()
    }

    pub(crate) fn fd(&self, index: usize) -> Option<RawFd> {
        self.slots.get(index)?.client.pending_fd()
    }

    pub(crate) fn wants_write(&self, index: usize) -> bool {
        self.slots
            .get(index)
            .is_some_and(|slot| slot.client.pending_wants_write())
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.slots
            .iter()
            .filter_map(|slot| match slot.state {
                State::Queued { retry_at } => Some(retry_at),
                State::Active { deadline } | State::VerifyActive { deadline, .. } => Some(deadline),
                State::VerifyQueued { .. } => Some(Instant::now()),
                _ => None,
            })
            .min()
    }

    pub(crate) fn busy(&self) -> bool {
        self.slots
            .iter()
            .any(|slot| !matches!(slot.state, State::Free))
    }

    pub(crate) fn replace_credentials(&mut self, credentials: super::Credentials) {
        assert!(!self.busy(), "credential rotation waits for upload owners");
        for slot in self.slots.iter_mut() {
            slot.client.replace_credentials(credentials);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;

    fn config(port: u16) -> Config {
        let mut config = Config::default_dev();
        config.object_store_on = true;
        config.object_store_endpoint = format!("127.0.0.1:{port}");
        config.object_store_bucket = "parallel-upload".to_string();
        config.object_store_access_key = "access".to_string();
        config.object_store_secret_key = "secret".to_string();
        config.object_store_put_slots = 2;
        config.object_store_head_bytes = 8192;
        config.object_store_response_bytes = 4096;
        config.wal_upload_buffer_bytes = 256 * 1024;
        config
    }

    fn request(stream: &mut TcpStream) -> Vec<u8> {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut part = [0; 8192];
        let end = loop {
            let n = stream.read(&mut part).unwrap();
            assert_ne!(n, 0);
            bytes.extend_from_slice(&part[..n]);
            if let Some(at) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                break at + 4;
            }
        };
        let head = core::str::from_utf8(&bytes[..end]).unwrap();
        assert!(head.starts_with("PUT "));
        assert!(head.contains("if-none-match: *\r\n"));
        assert!(head.contains("authorization: AWS4-HMAC-SHA256 "));
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length: "))
            .unwrap()
            .parse()
            .unwrap();
        while bytes.len() < end + length {
            let n = stream.read(&mut part).unwrap();
            assert_ne!(n, 0);
            bytes.extend_from_slice(&part[..n]);
        }
        bytes[end..].to_vec()
    }

    fn reply(stream: &mut TcpStream, etag: &str) {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nETag: \"{etag}\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
    }

    #[test]
    fn parallel_uploads_own_bytes_and_keep_out_of_order_completions() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let config = config(listener.local_addr().unwrap().port());
        let (release, held) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let (mut second, _) = listener.accept().unwrap();
            assert_eq!(request(&mut first), vec![b'a'; 192 * 1024]);
            assert_eq!(request(&mut second), vec![b'b'; 192 * 1024]);
            reply(&mut second, "second");
            held.recv_timeout(Duration::from_secs(5)).unwrap();
            reply(&mut first, "first");
        });
        let mut budget = Budget::new(Uploads::budget_bytes(&config));
        let mut uploads = Uploads::new(&config, &mut budget).unwrap();
        assert_eq!(budget.remaining(), 0);
        let mut bytes = vec![b'a'; 192 * 1024];
        let first = uploads
            .enqueue("first", &bytes, Condition::ImmutableCreate)
            .unwrap();
        bytes.fill(b'b');
        let second = uploads
            .enqueue("second", &bytes, Condition::ImmutableCreate)
            .unwrap();
        bytes.fill(b'x');
        crate::mem::guard::forbid_alloc(|| {
            assert!(matches!(
                uploads.enqueue("full", b"x", Condition::ImmutableCreate),
                Err(UploadError::SlotsExhausted)
            ));
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                assert!(
                    Instant::now() < deadline,
                    "second upload could not pass held first upload"
                );
                uploads.advance(Instant::now());
                assert!(uploads.take(first).unwrap().is_none());
                if let Some(etag) = uploads.take(second).unwrap() {
                    assert_eq!(etag.as_str(), "\"second\"");
                    break;
                }
                std::thread::yield_now();
            }
            let replacement = uploads
                .enqueue("replacement", b"new", Condition::ImmutableCreate)
                .unwrap();
            assert!(matches!(
                uploads.take(second),
                Err(UploadError::InvalidTicket)
            ));
            uploads.cancel(replacement).unwrap();
        });
        release.send(()).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(uploads.finish(first).unwrap().as_str(), "\"first\"");
            assert!(!uploads.busy());
            assert!(matches!(
                uploads.take(first),
                Err(UploadError::InvalidTicket)
            ));
        });
        server.join().unwrap();
    }

    #[test]
    fn parallel_uploads_reject_capacity_without_stealing_an_owner() {
        let config = config(1);
        let mut budget = Budget::new(Uploads::budget_bytes(&config));
        let mut uploads = Uploads::new(&config, &mut budget).unwrap();
        let body = vec![0; config.wal_upload_buffer_bytes + 1];
        let first = uploads
            .enqueue("held", b"held", Condition::ImmutableCreate)
            .unwrap();
        assert!(matches!(
            uploads.enqueue("too-large", &body, Condition::ImmutableCreate),
            Err(UploadError::BodyTooLarge { .. })
        ));
        let second = uploads
            .enqueue("still-free", b"ok", Condition::ImmutableCreate)
            .unwrap();
        assert!(uploads.take(first).unwrap().is_none());
        uploads.cancel(first).unwrap();
        uploads.cancel(second).unwrap();
        assert!(!uploads.busy());
    }

    #[test]
    fn parallel_uploads_cancel_drops_the_socket_and_frees_its_generation() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let config = config(listener.local_addr().unwrap().port());
        let mut budget = Budget::new(Uploads::budget_bytes(&config));
        let mut uploads = Uploads::new(&config, &mut budget).unwrap();
        let first = uploads
            .enqueue("uncertain", b"body", Condition::ImmutableCreate)
            .unwrap();
        uploads.advance(Instant::now());
        assert!(uploads.fd(first.slot).is_some());
        uploads.cancel(first).unwrap();
        assert!(uploads.fd(first.slot).is_none());
        let second = uploads
            .enqueue("next", b"next", Condition::ImmutableCreate)
            .unwrap();
        assert_ne!(first, second);
        assert!(matches!(
            uploads.cancel(first),
            Err(UploadError::InvalidTicket)
        ));
        assert!(uploads.take(second).unwrap().is_none());
        uploads.cancel(second).unwrap();
    }
    #[test]
    fn parallel_uploads_verify_uncertain_creates_beyond_response_capacity() {
        let mut config = config(1);
        config.object_store_sim = true;
        config.object_store_bucket = format!("verify-upload-{}", std::process::id());
        super::super::sim::drop_namespace(&config.object_store_bucket);
        let mut budget = Budget::new(Uploads::budget_bytes(&config));
        let mut uploads = Uploads::new(&config, &mut budget).unwrap();
        for length in [0, 4095, 4096, 4097, 8192, 8193] {
            let key = format!("value-{length}");
            let bytes = vec![b'x'; length];
            let created = uploads
                .enqueue(&key, &bytes, Condition::ImmutableCreate)
                .unwrap();
            let original = uploads.finish(created).unwrap();
            let retry = uploads
                .enqueue(&key, &bytes, Condition::ImmutableCreate)
                .unwrap();
            assert_eq!(uploads.finish(retry).unwrap(), original);
            let mut collision = bytes.clone();
            collision.push(b'y');
            let wrong_length = uploads
                .enqueue(&key, &collision, Condition::ImmutableCreate)
                .unwrap();
            assert!(matches!(
                uploads.finish(wrong_length),
                Err(UploadError::Request(Error::Protocol(
                    "immutable object collision"
                )))
            ));
            if length != 0 {
                collision[length - 1] = b'z';
                let wrong_bytes = uploads
                    .enqueue(&key, &collision[..length], Condition::ImmutableCreate)
                    .unwrap();
                assert!(matches!(
                    uploads.finish(wrong_bytes),
                    Err(UploadError::Request(Error::Protocol(
                        "immutable object collision"
                    )))
                ));
            }
        }
        assert!(!uploads.busy());
        drop(uploads);
        super::super::sim::drop_namespace(&config.object_store_bucket);
    }
    #[test]
    fn parallel_uploads_reject_invalid_startup_capacities() {
        for invalid in 0..3 {
            let mut config = config(1);
            let expected = match invalid {
                0 => {
                    config.object_store_put_slots = 1;
                    "object_store_put_slots must be at least 2"
                }
                1 => {
                    config.wal_upload_buffer_bytes = 0;
                    "wal_upload_buffer_bytes must be greater than zero"
                }
                _ => {
                    config.object_store_response_bytes = 0;
                    "object_store_response_bytes must be greater than zero"
                }
            };
            let mut budget = Budget::new(Uploads::budget_bytes(&config));
            let error = match Uploads::new(&config, &mut budget) {
                Ok(_) => panic!("invalid upload capacity accepted"),
                Err(error) => error,
            };
            assert_eq!(error.to_string(), expected);
        }
    }
}
