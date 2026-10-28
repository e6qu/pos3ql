//! One startup-reserved metadata request, retained through reactor suspension.

use std::task::Poll;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::mem::budget::Budget;
use crate::mem::fixed_vec::FixedVec;
use crate::object_store::{Client, Credentials, Error, ListCursor, MAX_OBJECT_KEY_BYTES};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use crate::util::StackStr;

pub(super) const PAGE_KEYS: usize = 64;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Operation {
    Get(StackStr<128>),
    Delete(StackStr<128>),
    List {
        prefix: StackStr<128>,
        cursor: ListCursor,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    fn network() -> (MetadataIo, TcpListener) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut config = Config::default_dev();
        config.object_store_on = true;
        config.object_store_sim = false;
        config.object_store_endpoint = listener.local_addr().unwrap().to_string();
        config.object_store_bucket = "maintenance-network".to_owned();
        config.object_store_prefix = "db space/".to_owned();
        config.object_store_access_key = "test-access".to_owned();
        config.object_store_secret_key = "test-secret".to_owned();
        let mut budget = Budget::new(MetadataIo::budget_bytes(&config));
        let io = MetadataIo::new(&config, &mut budget).unwrap();
        assert_eq!(budget.remaining(), 0);
        (io, listener)
    }

    fn accept(io: &mut MetadataIo, listener: &TcpListener) -> (TcpStream, String) {
        let deadline = Instant::now() + Duration::from_secs(2);
        let (mut stream, _) = loop {
            io.advance(Instant::now());
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "metadata request did not connect"
                    );
                    std::thread::yield_now();
                }
                Err(error) => panic!("accept metadata request: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            bytes.push(byte[0]);
            assert!(bytes.len() <= 8192);
        }
        (stream, String::from_utf8(bytes).unwrap())
    }

    fn finish(io: &mut MetadataIo) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !io.ready() {
            crate::mem::guard::forbid_alloc(|| io.advance(Instant::now()));
            assert!(
                Instant::now() < deadline,
                "metadata response did not finish"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn maintenance_metadata_waits_retain_requests_pages_and_idempotent_deletes() {
        let (mut io, listener) = network();
        let mut observed = Vec::new();
        assert!(
            crate::mem::guard::forbid_alloc(
                || io.list("commits/", ListCursor::START, |_| panic!("unfinished page"))
            )
            .unwrap()
            .is_pending()
        );
        let (mut stream, request) = accept(&mut io, &listener);
        assert!(request.starts_with("GET /maintenance-network?encoding-type=url&list-type=2&max-keys=64&prefix=db%20space%2Fcommits%2F HTTP/1.1"));
        assert!(!io.ready());
        assert!(io.busy());
        assert!(io.fd().is_some());
        let wrong = io.get("commits/wrong.head").unwrap_err();
        assert_eq!(wrong.sqlstate, sqlstate::INTERNAL_ERROR);
        assert!(!io.ready());
        let body = "<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken>page&amp;two</NextContinuationToken><Contents><Key>db%20space%2Fcommits%2Fa.head</Key></Contents></ListBucketResult>";
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            &body[..30]
        )
        .unwrap();
        crate::mem::guard::forbid_alloc(|| io.advance(Instant::now()));
        assert!(!io.ready());
        assert!(
            io.list("commits/", ListCursor::START, |_| panic!("partial page"))
                .unwrap()
                .is_pending()
        );
        stream.write_all(body[30..].as_bytes()).unwrap();
        finish(&mut io);
        let Poll::Ready(Some(cursor)) = io
            .list("commits/", ListCursor::START, |key| {
                observed.push(key.to_owned())
            })
            .unwrap()
        else {
            panic!("completed page");
        };
        assert_eq!(observed, ["commits/a.head"]);
        assert_eq!(cursor.token.unwrap().as_str(), "page&two");
        assert_eq!(cursor.after.as_str(), "commits/a.head");
        assert!(!io.busy());
        assert!(
            io.list("commits/", cursor, |_| panic!("unfinished second page"))
                .unwrap()
                .is_pending()
        );
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            io.advance(Instant::now());
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        assert!(
            String::from_utf8(request)
                .unwrap()
                .contains("continuation-token=page%26two&")
        );
        let body = "<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>db%20space%2Fcommits%2Fb.batch</Key></Contents></ListBucketResult>";
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        finish(&mut io);
        let mut count = 0;
        assert_eq!(
            crate::mem::guard::forbid_alloc(|| io.list("commits/", cursor, |key| {
                assert_eq!(key, "commits/b.batch");
                count += 1;
            }))
            .unwrap(),
            Poll::Ready(None)
        );
        assert_eq!(count, 1);
        assert!(io.delete("commits/b.batch").unwrap().is_pending());
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            io.advance(Instant::now());
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        assert!(request.starts_with(b"DELETE /maintenance-network/db%20space/commits/b.batch "));
        stream
            .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n")
            .unwrap();
        finish(&mut io);
        assert_eq!(
            crate::mem::guard::forbid_alloc(|| io.delete("commits/b.batch")).unwrap(),
            Poll::Ready(())
        );
        assert!(!io.busy());
    }

    #[test]
    fn maintenance_metadata_deadlines_and_credential_drain_release_owned_requests() {
        let (mut io, listener) = network();
        assert!(io.get("backups/saved/manifest").unwrap().is_pending());
        let (_stream, request) = accept(&mut io, &listener);
        assert!(request.starts_with("GET /maintenance-network/db%20space/backups/saved/manifest "));
        let deadline = io.deadline().unwrap();
        crate::mem::guard::forbid_alloc(|| io.advance(deadline));
        assert!(io.ready());
        assert!(io.fd().is_none());
        let error = io.get("backups/saved/manifest").unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::IO_ERROR);
        assert!(!io.busy());
        assert!(io.get("backups/saved/manifest").unwrap().is_pending());
        let (mut stream, _) = accept(&mut io, &listener);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\netag: \"manifest\"\r\ncontent-length: 5\r\n\r\nsaved")
            .unwrap();
        finish(&mut io);
        crate::mem::guard::forbid_alloc(|| {
            let Poll::Ready(bytes) = io.get("backups/saved/manifest").unwrap() else {
                panic!("GET complete");
            };
            assert_eq!(bytes, b"saved");
            io.set_admission(crate::object_store::RequestAdmission::Draining);
            assert!(io.get("backups/other/manifest").unwrap().is_pending());
            assert!(!io.busy());
            io.replace_credentials(Credentials::new("replacement", "secret", "").unwrap());
            io.set_admission(crate::object_store::RequestAdmission::Open);
            assert!(io.delete("commits/old.batch").unwrap().is_pending());
            io.cancel();
            assert!(!io.busy());
        });
    }
}

enum Response {
    Get,
    Delete,
    List(Option<ListCursor>),
}

#[allow(
    clippy::large_enum_variant,
    reason = "fixed request state is charged at startup"
)]
enum State {
    Idle,
    Active {
        operation: Operation,
        deadline: Instant,
    },
    Complete {
        operation: Operation,
        result: Result<Response, Error>,
    },
}

pub(super) struct MetadataIo {
    client: Client,
    state: State,
    keys: FixedVec<StackStr<MAX_OBJECT_KEY_BYTES>>,
    admission: crate::object_store::RequestAdmission,
}

impl MetadataIo {
    pub(super) fn budget_bytes(config: &Config) -> usize {
        Client::budget_bytes(config)
            + core::mem::size_of::<State>()
            + PAGE_KEYS * core::mem::size_of::<StackStr<MAX_OBJECT_KEY_BYTES>>()
    }

    pub(super) fn new(
        config: &Config,
        budget: &mut Budget,
    ) -> Result<Self, super::CheckpointSetupError> {
        budget
            .draw(core::mem::size_of::<State>(), "maintenance request state")
            .map_err(super::CheckpointSetupError::Budget)?;
        let mut client = Client::new(config, budget)
            .map_err(|error| super::CheckpointSetupError::ObjectStore(error.to_string()))?;
        client.enable_async_gets();
        Ok(Self {
            client,
            state: State::Idle,
            keys: FixedVec::new(budget, "maintenance page keys", PAGE_KEYS)
                .map_err(super::CheckpointSetupError::Budget)?,
            admission: crate::object_store::RequestAdmission::Open,
        })
    }

    fn key(key: &str) -> Result<StackStr<128>, SqlError> {
        if key.len() > 128 {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "maintenance object key exceeds 128 bytes"
            ));
        }
        Ok(StackStr::from_str(key))
    }

    fn poll(&mut self, operation: Operation) -> Result<Poll<Response>, SqlError> {
        if matches!(self.state, State::Idle) {
            if self.admission == crate::object_store::RequestAdmission::Draining {
                return Ok(Poll::Pending);
            }
            self.keys.clear();
            self.state = State::Active {
                operation,
                deadline: Instant::now() + REQUEST_TIMEOUT,
            };
        }
        let retained = match &self.state {
            State::Active { operation, .. } | State::Complete { operation, .. } => *operation,
            State::Idle => unreachable!(),
        };
        if retained != operation {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "maintenance request changed before its result was consumed"
            ));
        }
        self.advance(Instant::now());
        if matches!(self.state, State::Complete { .. }) {
            let State::Complete { result, .. } = core::mem::replace(&mut self.state, State::Idle)
            else {
                unreachable!()
            };
            return result.map(Poll::Ready).map_err(super::object_store_to_sql);
        }
        Ok(Poll::Pending)
    }

    pub(super) fn get(&mut self, key: &str) -> Result<Poll<&[u8]>, SqlError> {
        match self.poll(Operation::Get(Self::key(key)?))? {
            Poll::Ready(Response::Get) => Ok(Poll::Ready(self.client.body_bytes())),
            Poll::Pending => Ok(Poll::Pending),
            _ => unreachable!("matching metadata operation"),
        }
    }

    pub(super) fn delete(&mut self, key: &str) -> Result<Poll<()>, SqlError> {
        match self.poll(Operation::Delete(Self::key(key)?))? {
            Poll::Ready(Response::Delete) => Ok(Poll::Ready(())),
            Poll::Pending => Ok(Poll::Pending),
            _ => unreachable!("matching metadata operation"),
        }
    }

    pub(super) fn list(
        &mut self,
        prefix: &str,
        cursor: ListCursor,
        mut each: impl FnMut(&str),
    ) -> Result<Poll<Option<ListCursor>>, SqlError> {
        match self.poll(Operation::List {
            prefix: Self::key(prefix)?,
            cursor,
        })? {
            Poll::Ready(Response::List(next)) => {
                for key in self.keys.iter() {
                    each(key.as_str());
                }
                self.keys.clear();
                Ok(Poll::Ready(next))
            }
            Poll::Pending => Ok(Poll::Pending),
            _ => unreachable!("matching metadata operation"),
        }
    }

    pub(super) fn advance(&mut self, now: Instant) {
        let State::Active {
            operation,
            deadline,
        } = self.state
        else {
            return;
        };
        let result = if now >= deadline {
            Err(Error::Io {
                context: "maintenance request deadline",
                kind: std::io::ErrorKind::TimedOut,
                detail: StackStr::new(),
            })
        } else {
            match operation {
                Operation::Get(key) => self.client.get(key.as_str(), None).map(|_| Response::Get),
                Operation::Delete(key) => self
                    .client
                    .delete_nonblocking(key.as_str())
                    .map(|()| Response::Delete),
                Operation::List { prefix, cursor } => {
                    self.keys.clear();
                    let keys = &mut self.keys;
                    self.client
                        .list_page(prefix.as_str(), cursor, PAGE_KEYS, |key| {
                            keys.push(StackStr::from_str(key))
                                .expect("validated page key limit");
                        })
                        .map(Response::List)
                }
            }
        };
        if !matches!(result, Err(Error::WouldBlock)) {
            if result.is_err() {
                self.client.cancel_request();
                self.keys.clear();
            }
            self.state = State::Complete { operation, result };
        }
    }

    pub(super) fn busy(&self) -> bool {
        !matches!(self.state, State::Idle)
    }
    pub(super) fn ready(&self) -> bool {
        !matches!(self.state, State::Active { .. })
    }
    pub(super) fn deadline(&self) -> Option<Instant> {
        match self.state {
            State::Active { deadline, .. } => Some(deadline),
            _ => None,
        }
    }
    pub(super) fn fd(&self) -> Option<std::os::fd::RawFd> {
        self.client.pending_fd()
    }
    pub(super) fn wants_write(&self) -> bool {
        self.client.pending_wants_write()
    }
    pub(super) fn cancel(&mut self) {
        self.client.cancel_request();
        self.keys.clear();
        self.state = State::Idle;
    }
    /// Explicit checkpoint/shutdown callers have no reactor to drive the socket.
    pub(super) fn wait_ready(&mut self) -> Result<(), SqlError> {
        let Some(deadline) = self.deadline() else {
            return Ok(());
        };
        if let Some(fd) = self.fd() {
            let mut descriptor = libc::pollfd {
                fd,
                events: libc::POLLIN | if self.wants_write() { libc::POLLOUT } else { 0 },
                revents: 0,
            };
            let timeout = deadline
                .saturating_duration_since(Instant::now())
                .as_nanos()
                .div_ceil(1_000_000)
                .min(i32::MAX as u128) as i32;
            if unsafe { libc::poll(&mut descriptor, 1, timeout) } < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(super::object_store_to_sql(Error::Io {
                        context: "maintenance request readiness",
                        kind: error.kind(),
                        detail: StackStr::new(),
                    }));
                }
            }
        }
        self.advance(Instant::now());
        Ok(())
    }
    pub(super) fn set_admission(&mut self, admission: crate::object_store::RequestAdmission) {
        self.admission = admission;
    }
    pub(super) fn replace_credentials(&mut self, credentials: Credentials) {
        assert!(
            !self.busy(),
            "maintenance retains its signed request through consumption"
        );
        self.client.replace_credentials(credentials);
    }
}
