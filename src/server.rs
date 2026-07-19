//! The single-threaded server: one reactor, a fixed array of connection
//! slots whose buffers are allocated once at startup, and the query engine.

use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::time::Duration;

use crate::config::Config;
use crate::io::reactor::Reactor;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::pg::auth::{AuthMode, ScramServer, SCRAM_ITERATIONS};
use crate::pg::conn::{After, AuthContext, Conn};
use crate::sql::Engine;

const LISTENER_TOKEN: u64 = u64::MAX;
const SHUTDOWN_TOKEN: u64 = u64::MAX - 1;

/// Set by the signal handler; the loop drains and exits when it sees this.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
/// Write end of the self-pipe, written by the signal handler to wake the
/// reactor. -1 until installed.
static SHUTDOWN_PIPE_WRITE: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(-1);

extern "C" fn on_signal(_sig: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
    let fd = SHUTDOWN_PIPE_WRITE.load(Ordering::SeqCst);
    if fd >= 0 {
        let byte = [1u8];
        // Async-signal-safe: a single write of one byte.
        unsafe {
            libc::write(fd, byte.as_ptr().cast(), 1);
        }
    }
}

pub struct Server {
    reactor: Reactor,
    listener: TcpListener,
    slots: FixedVec<Slot>,
    free: FixedVec<u32>,
    engine: Engine,
    /// Random key sent in BackendKeyData (16 bytes; protocol 3.0 gets the
    /// first 4). Cancellation itself is not implemented yet.
    cancel_key: [u8; 16],
    next_conn_id: i32,
    /// Pre-rendered "too many connections" ErrorResponse for refusals.
    refusal: ([u8; 128], usize),
    auth: AuthContext,
    /// Read end of the shutdown self-pipe.
    shutdown_read: i32,
}

struct Slot {
    conn: Conn,
    generation: u32,
    want_write: bool,
}

#[derive(Debug)]
pub enum ServerSetupError {
    Budget(BudgetError),
    Io(&'static str, std::io::Error),
    Engine(crate::sql::EngineSetupError),
}

impl std::fmt::Display for ServerSetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Budget(e) => write!(f, "{e}"),
            Self::Io(what, e) => write!(f, "{what}: {e}"),
            Self::Engine(e) => write!(f, "{e}"),
        }
    }
}

impl From<crate::sql::EngineSetupError> for ServerSetupError {
    fn from(e: crate::sql::EngineSetupError) -> Self {
        Self::Engine(e)
    }
}

impl std::error::Error for ServerSetupError {}

impl From<BudgetError> for ServerSetupError {
    fn from(e: BudgetError) -> Self {
        Self::Budget(e)
    }
}

impl Server {
    pub fn new(config: &Config, budget: &mut Budget) -> Result<Self, ServerSetupError> {
        let max_conns = config.max_connections as usize;
        let listener = TcpListener::bind(&config.listen_addr)
            .map_err(|e| ServerSetupError::Io("bind listen_addr", e))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| ServerSetupError::Io("set listener nonblocking", e))?;

        let reactor = Reactor::new(budget, max_conns + 1)
            .map_err(|e| match e {
                crate::io::reactor::ReactorSetupError::Budget(b) => ServerSetupError::Budget(b),
                crate::io::reactor::ReactorSetupError::Os(io) => {
                    ServerSetupError::Io("create kqueue", io)
                }
            })?;
        reactor
            .register_read(listener.as_raw_fd(), LISTENER_TOKEN)
            .map_err(|e| ServerSetupError::Io("register listener", e))?;

        let mut slots = FixedVec::new(budget, "conn_slots", max_conns)?;
        let mut free = FixedVec::new(budget, "conn_free_list", max_conns)?;
        for i in (0..max_conns as u32).rev() {
            slots
                .push(Slot {
                    conn: Conn::new(config, budget)?,
                    generation: 0,
                    want_write: false,
                })
                .expect("sized to max_conns");
            free.push(i).expect("sized to max_conns");
        }

        let mut cancel_key = [0u8; 16];
        let rc = unsafe {
            libc::getentropy(cancel_key.as_mut_ptr().cast(), cancel_key.len())
        };
        if rc != 0 {
            return Err(ServerSetupError::Io(
                "getentropy for cancel key",
                std::io::Error::last_os_error(),
            ));
        }

        let refusal = Self::render_refusal(budget)?;
        let engine = Engine::new(config, budget)?;

        // Self-pipe for graceful shutdown, woken by the signal handler.
        let mut pipe_fds = [0i32; 2];
        if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
            return Err(ServerSetupError::Io(
                "shutdown pipe",
                std::io::Error::last_os_error(),
            ));
        }
        // Non-blocking read end.
        unsafe {
            let flags = libc::fcntl(pipe_fds[0], libc::F_GETFL);
            libc::fcntl(pipe_fds[0], libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        SHUTDOWN_PIPE_WRITE.store(pipe_fds[1], Ordering::SeqCst);
        reactor
            .register_read_oneshot(pipe_fds[0], SHUTDOWN_TOKEN)
            .map_err(|e| ServerSetupError::Io("register shutdown pipe", e))?;
        // Install handlers for SIGTERM and SIGINT.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_signal as *const () as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
            libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        }

        let mode = match config.auth.as_str() {
            "trust" => AuthMode::Trust,
            "password" => AuthMode::Password,
            "scram-sha-256" => AuthMode::ScramSha256,
            other => {
                return Err(ServerSetupError::Io(
                    "auth",
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("unknown auth mode '{other}'"),
                    ),
                ))
            }
        };
        if mode != AuthMode::Trust && config.password.is_empty() {
            return Err(ServerSetupError::Io(
                "auth",
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "auth requires a password in the config",
                ),
            ));
        }
        let scram = if mode == AuthMode::ScramSha256 {
            let mut salt = [0u8; 16];
            let rc = unsafe { libc::getentropy(salt.as_mut_ptr().cast(), salt.len()) };
            if rc != 0 {
                return Err(ServerSetupError::Io(
                    "getentropy for scram salt",
                    std::io::Error::last_os_error(),
                ));
            }
            Some(ScramServer::derive(&config.password, salt, SCRAM_ITERATIONS))
        } else {
            None
        };
        let auth = AuthContext {
            mode,
            password: config.password.clone(),
            scram,
        };

        Ok(Self {
            reactor,
            listener,
            slots,
            free,
            engine,
            cancel_key,
            next_conn_id: 1,
            refusal,
            auth,
            shutdown_read: pipe_fds[0],
        })
    }

    /// Builds the canned ErrorResponse sent when all slots are taken.
    fn render_refusal(budget: &mut Budget) -> Result<([u8; 128], usize), ServerSetupError> {
        use crate::pg::respond::Responder;
        let mut buffer = crate::mem::buffer::FixedBuf::new(budget, "refusal_scratch", 128)?;
        let mut responder = Responder::new(&mut buffer);
        responder.error(
            crate::sql::eval::sqlstate::TOO_MANY_CONNECTIONS,
            "sorry, too many clients already",
        )
        .expect("refusal fits in 128 bytes");
        let mut bytes = [0u8; 128];
        let n = buffer.readable().len();
        bytes[..n].copy_from_slice(buffer.readable());
        Ok((bytes, n))
    }

    /// The event loop. Runs until SIGTERM/SIGINT, then drains connections,
    /// takes a final checkpoint, and returns cleanly.
    pub fn run(&mut self) -> std::io::Result<()> {
        // Backoff for the asynchronous WAL-upload drain: zero while healthy
        // (drain eagerly between events), one second after a failure so a
        // persistently-unreachable bucket cannot spin the loop.
        let mut upload_backoff = Duration::ZERO;
        while !SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
            // While committed WAL awaits asynchronous upload, poll with the
            // backoff timeout so the loop returns to drain it; otherwise block
            // until the next event.
            let timeout = if self.engine.has_pending_wal_upload() {
                Some(upload_backoff)
            } else {
                None
            };
            let n = self.reactor.poll(timeout)?;
            for i in 0..n {
                let event = self.reactor.event(i);
                if event.token == SHUTDOWN_TOKEN {
                    // Drain the pipe; the flag is already set.
                    let mut buffer = [0u8; 64];
                    while unsafe {
                        libc::read(self.shutdown_read, buffer.as_mut_ptr().cast(), buffer.len())
                    } > 0
                    {}
                } else if event.token == LISTENER_TOKEN {
                    self.accept_pending();
                } else {
                    self.dispatch(event.token, event.readable, event.writable);
                }
            }
            // Upload committed WAL offset the commit path so request handling and
            // S3 latency never gate each other; back offset if the bucket errors.
            if self.engine.has_pending_wal_upload() {
                upload_backoff = if self.engine.drain_wal_upload() {
                    Duration::ZERO
                } else {
                    Duration::from_secs(1)
                };
            }
        }
        self.shutdown();
        Ok(())
    }

    /// Graceful shutdown: stop accepting, roll back in-flight transactions,
    /// close connections, take a final checkpoint. Runs post-freeze, so it
    /// must not allocate — messages go to stderr via raw writes.
    fn shutdown(&mut self) {
        stderr_line(b"pos3ql: shutdown requested, draining
");
        let _ = self.reactor.deregister(self.listener.as_raw_fd());
        for i in 0..self.slots.len() {
            if self.slots[i].conn.is_open() {
                let slot = &mut self.slots[i];
                self.engine.rollback_txn(&mut slot.conn.txn);
                self.release(i);
            }
        }
        match self.engine.checkpoint() {
            Ok(true) => stderr_line(b"pos3ql: final checkpoint written
"),
            Ok(false) => {}
            Err(_) => stderr_line(b"pos3ql: final checkpoint failed; journal is durable
"),
        }
        // Ensure the journal is durable even if no checkpoint ran.
        self.engine.commit_wal();
        stderr_line(b"pos3ql: shutdown complete
");
    }

    fn accept_pending(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((stream, _peer)) => self.admit(stream),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    log_io("accept", &e);
                    return;
                }
            }
        }
    }

    fn admit(&mut self, stream: TcpStream) {
        if let Err(e) = stream.set_nonblocking(true) {
            log_io("set_nonblocking", &e);
            return;
        }
        let _ = stream.set_nodelay(true);
        let Some(index) = self.free.pop() else {
            // Best-effort refusal; the startup response is small enough
            // that a fresh socket buffer will take it without blocking.
            use std::io::Write;
            let mut s = stream;
            let (bytes, n) = &self.refusal;
            let _ = s.write(&bytes[..*n]);
            return;
        };
        let slot = &mut self.slots[index as usize];
        let id = self.next_conn_id;
        self.next_conn_id = self.next_conn_id.wrapping_add(1).max(1);
        let fd = stream.as_raw_fd();
        slot.conn.open(stream, id);
        slot.want_write = false;
        let token = token_for(index, slot.generation);
        if let Err(e) = self.reactor.register_read(fd, token) {
            log_io("register connection", &e);
            slot.conn.close();
            slot.generation = slot.generation.wrapping_add(1);
            self.free.push(index).expect("slot was just taken");
        }
    }

    fn dispatch(&mut self, token: u64, readable: bool, writable: bool) {
        let index = (token & 0xffff_ffff) as usize;
        let generation = (token >> 32) as u32;
        if index >= self.slots.len() {
            return;
        }
        let slot = &mut self.slots[index];
        if slot.generation != generation || !slot.conn.is_open() {
            // Stale event for a slot that was already recycled.
            return;
        }
        let after = if readable {
            slot.conn.on_readable(&mut self.engine, &self.cancel_key, &self.auth)
        } else if writable {
            slot.conn.on_writable()
        } else {
            After::Continue
        };
        match after {
            After::Close => {
                // A dropped connection releases its uncommitted work.
                let slot = &mut self.slots[index];
                self.engine.rollback_txn(&mut slot.conn.txn);
                self.release(index)
            }
            After::Continue => {
                let slot = &mut self.slots[index];
                let desired = slot.conn.wants_write();
                if desired != slot.want_write {
                    let fd = slot.conn.stream().as_raw_fd();
                    let token = token_for(index as u32, slot.generation);
                    match self.reactor.set_write_interest(fd, token, desired) {
                        Ok(()) => slot.want_write = desired,
                        Err(e) => {
                            log_io("set write interest", &e);
                            self.release(index);
                        }
                    }
                }
            }
        }
    }

    fn release(&mut self, index: usize) {
        let slot = &mut self.slots[index];
        if let Some(stream) = slot.conn.close() {
            // Closing the fd drops its kqueue registrations; an explicit
            // deregister first keeps the reactor's view tidy and catches
            // double-release bugs in debug runs.
            let _ = self.reactor.deregister(stream.as_raw_fd());
            drop(stream);
        }
        slot.generation = slot.generation.wrapping_add(1);
        slot.want_write = false;
        self.free
            .push(index as u32)
            .expect("released slot cannot exceed capacity");
    }

    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }
}

/// Allocation-free stderr write for the post-freeze shutdown path.
fn stderr_line(msg: &[u8]) {
    unsafe {
        libc::write(2, msg.as_ptr().cast(), msg.len());
    }
}

fn token_for(index: u32, generation: u32) -> u64 {
    (u64::from(generation) << 32) | u64::from(index)
}

/// Post-freeze-safe logging: io::Error's Display allocates (strerror into a
/// String), so only the kind and raw code are printed.
fn log_io(context: &str, e: &std::io::Error) {
    eprintln!(
        "pos3ql: {context}: kind={:?} os_error={:?}",
        e.kind(),
        e.raw_os_error()
    );
}
