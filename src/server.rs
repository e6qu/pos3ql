//! The single-threaded server: one reactor, a fixed array of connection
//! slots whose buffers are allocated once at startup, and the query engine.

use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::fd::{AsRawFd, FromRawFd};
use std::time::Duration;

use crate::config::Config;
use crate::io::reactor::Reactor;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::pg::auth::{AuthMode, SCRAM_ITERATIONS, ScramServer};
use crate::pg::conn::{After, AuthContext, Conn};
use crate::sql::Engine;

const LISTENER_TOKEN: u64 = u64::MAX;
const SHUTDOWN_TOKEN: u64 = u64::MAX - 1;
/// First reactor token for durable block-store in-flight GET sockets.
const BLOCK_IO_TOKEN: u64 = u64::MAX - 2;

/// Set by the signal handler; the loop drains and exits when it sees this.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
/// Write end of the self-pipe, written by the signal handler to wake the
/// reactor. -1 until installed.
static SHUTDOWN_PIPE_WRITE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

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
    /// Server-side TLS configuration, built at startup when `tls_on`.
    tls_config: Option<std::sync::Arc<rustls::ServerConfig>>,
    /// Read end of the shutdown self-pipe.
    shutdown_read: i32,
    /// One registered socket per fixed durable-block GET slot.
    block_read_fds: FixedVec<Option<i32>>,
}

struct Slot {
    conn: Conn,
    generation: u32,
    want_read: bool,
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
        let listener = bind_listener(&config.listen_addr)
            .map_err(|e| ServerSetupError::Io("bind listen_addr", e))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| ServerSetupError::Io("set listener nonblocking", e))?;

        let block_read_slots = if config.object_store_on {
            config.object_store_get_slots
        } else {
            0
        };
        #[allow(
            unused_mut,
            reason = "the Linux epoll backend records fixed read/write interest"
        )]
        let mut reactor =
            Reactor::new(budget, max_conns + 1 + block_read_slots).map_err(|e| match e {
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
        let mut block_read_fds = FixedVec::new(budget, "block_read_fds", block_read_slots)?;
        for _ in 0..block_read_slots {
            block_read_fds
                .push(None)
                .expect("sized from object_store_get_slots");
        }
        for i in (0..max_conns as u32).rev() {
            slots
                .push(Slot {
                    conn: Conn::new(config, budget)?,
                    generation: 0,
                    want_read: false,
                    want_write: false,
                })
                .expect("sized to max_conns");
            free.push(i).expect("sized to max_conns");
        }

        let mut cancel_key = [0u8; 16];
        let rc = unsafe { libc::getentropy(cancel_key.as_mut_ptr().cast(), cancel_key.len()) };
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
                ));
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
            Some(ScramServer::derive(
                &config.password,
                salt,
                SCRAM_ITERATIONS,
            ))
        } else {
            None
        };
        let auth = AuthContext {
            mode,
            password: config.password.clone(),
            scram,
        };

        // Built here, before the allocator freezes, so its startup allocations
        // are free; runtime session work is charged to the TLS pool.
        let tls_config = if config.tls_on {
            Some(
                crate::pg::tls::build_server_config(&config.tls_cert_file, &config.tls_key_file)
                    .map_err(|e| ServerSetupError::Io("tls", std::io::Error::other(e)))?,
            )
        } else {
            None
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
            tls_config,
            shutdown_read: pipe_fds[0],
            block_read_fds,
        })
    }

    /// Builds the canned ErrorResponse sent when all slots are taken.
    fn render_refusal(budget: &mut Budget) -> Result<([u8; 128], usize), ServerSetupError> {
        use crate::pg::respond::Responder;
        let mut buffer = crate::mem::buffer::FixedBuf::new(budget, "refusal_scratch", 128)?;
        let mut responder = Responder::new(&mut buffer);
        responder
            .error(
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
        self.engine.enable_async_block_reads();
        // Checkpoint beats run eagerly while healthy and back off for one
        // second after an object-store failure.
        let mut beat_backoff = Duration::ZERO;
        while !SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
            // While a checkpoint sweep is mid-flight, poll with the backoff
            // timeout so the loop returns to that work; otherwise block until
            // the next event.
            let checkpoint_timeout = if self.block_read_fds.iter().all(Option::is_none)
                && self.engine.checkpoint_work_pending()
            {
                Some(beat_backoff)
            } else {
                None
            };
            let hedge_timeout = self
                .engine
                .next_block_read_hedge_deadline()
                .map(|deadline| deadline.saturating_duration_since(std::time::Instant::now()));
            let timeout = [
                checkpoint_timeout,
                self.next_lock_wait_timeout(),
                hedge_timeout,
            ]
            .into_iter()
            .flatten()
            .min();
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
                } else if let Some(slot) = self.block_slot(event.token) {
                    self.advance_block_io(slot)?;
                } else {
                    self.dispatch(event.token, event.readable, event.writable);
                }
            }
            self.pump_replication_streams();
            // A lock timeout can be the event that woke the reactor, with no
            // socket readiness and no lock-generation change.
            self.wake_lock_waiters();
            self.engine
                .issue_due_block_read_hedges(std::time::Instant::now());
            if self.block_read_fds.iter().all(Option::is_none) {
                self.engine.enable_async_block_reads();
            }
            // Enabling queued reads may open their non-blocking GET sockets.
            // Reconcile after it does so every pending read has reactor
            // interest before this loop can block again.
            self.sync_block_read_interest()?;
            // Active checkpoint and compaction work advances even on an
            // idle server — a trigger must not wait for the next client
            // message to finish what it started, and a merge owes its beats
            // regardless of traffic. One beat per loop turn, backing off
            // when the bucket errors.
            if self.engine.checkpoint_work_pending() {
                beat_backoff = if self.engine.maybe_checkpoint() {
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
        stderr_line(
            b"pos3ql: shutdown requested, draining
",
        );
        let _ = self.reactor.deregister(self.listener.as_raw_fd());
        for i in 0..self.slots.len() {
            if self.slots[i].conn.is_open() {
                let slot = &mut self.slots[i];
                self.engine.rollback_txn(&mut slot.conn.txn, &slot.conn.guc);
                self.release(i);
            }
        }
        if self.engine.checkpoint_enabled() {
            match self.engine.checkpoint() {
                Ok(true) => stderr_line(
                    b"pos3ql: final checkpoint written
",
                ),
                Ok(false) => {}
                Err(_) => stderr_line(
                    b"pos3ql: final checkpoint failed; journal is durable
",
                ),
            }
        }
        // Ensure the journal is durable even if no checkpoint ran.
        if self.engine.commit_wal().is_err() {
            stderr_line(b"pos3ql: final WAL upload failed\n");
        }
        stderr_line(
            b"pos3ql: shutdown complete
",
        );
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
        slot.want_read = true;
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
            slot.conn.on_readable(
                &mut self.engine,
                &self.cancel_key,
                &self.auth,
                self.tls_config.as_ref(),
            )
        } else if writable {
            slot.conn.on_writable()
        } else {
            After::Continue
        };
        match after {
            After::Close => {
                // A dropped connection releases its uncommitted work.
                let slot = &mut self.slots[index];
                self.engine.rollback_txn(&mut slot.conn.txn, &slot.conn.guc);
                self.release(index)
            }
            After::Continue => {
                let slot = &mut self.slots[index];
                let read_desired = slot.conn.wants_read();
                if read_desired != slot.want_read {
                    let fd = slot.conn.stream().as_raw_fd();
                    let token = token_for(index as u32, slot.generation);
                    match self.reactor.set_read_interest(fd, token, read_desired) {
                        Ok(()) => slot.want_read = read_desired,
                        Err(e) => {
                            log_io("set read interest", &e);
                            self.release(index);
                            return;
                        }
                    }
                }
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
        // A NOTIFY committed by the message just processed leaves notifications
        // in the engine outbox; fan them out to every listening connection.
        if self.engine.has_notifications() {
            self.deliver_notifications();
        }
    }

    fn next_lock_wait_timeout(&self) -> Option<Duration> {
        self.slots
            .iter()
            .filter_map(|slot| slot.conn.lock_wait_remaining())
            .min()
    }

    /// Called when the block-store client's non-blocking GET socket is
    /// readable. Advances the pending response read; if it completes, the
    /// block is now cached and any parked statement is retried.
    fn advance_block_io(&mut self, slot: usize) -> std::io::Result<()> {
        if self
            .engine
            .advance_pending_block_read(slot)
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::Other))?
        {
            // The GET completed and the block is cached. Wake only statements
            // that are parked on object I/O; lock waiters have a separate
            // generation-driven wakeup path.
            self.wake_io_waiters();
        }
        Ok(())
    }

    /// Reconciles every fixed object-read slot with the reactor. Registration
    /// failures surface from the loop; a parked query must never wait on an
    /// unobserved descriptor.
    fn sync_block_read_interest(&mut self) -> std::io::Result<()> {
        assert_eq!(self.block_read_fds.len(), self.engine.block_read_slots());
        for slot in 0..self.block_read_fds.len() {
            let wanted = self.engine.pending_block_read_fd(slot);
            match (self.block_read_fds[slot], wanted) {
                (Some(registered), Some(fd)) if registered == fd => {
                    // A completed or cancelled GET can close `registered` and
                    // the next connection may receive the same integer fd.
                    // EV_ADD is idempotent for a live registration and
                    // recreates the filter after that close.
                    self.reactor
                        .register_read(fd, BLOCK_IO_TOKEN - slot as u64)?;
                }
                (registered, wanted) => {
                    if let Some(fd) = registered {
                        self.reactor.deregister(fd)?;
                    }
                    if let Some(fd) = wanted {
                        self.reactor
                            .register_read(fd, BLOCK_IO_TOKEN - slot as u64)?;
                    }
                    self.block_read_fds[slot] = wanted;
                }
            }
        }
        Ok(())
    }

    fn block_slot(&self, token: u64) -> Option<usize> {
        let slot = BLOCK_IO_TOKEN.checked_sub(token)? as usize;
        (slot < self.block_read_fds.len()).then_some(slot)
    }

    /// Retries parked protocol messages after a transaction released row
    /// locks. Each connection retains its frontend message and simple-query
    /// statement index, so wakeup neither reparses client state nor replays
    /// completed commands.
    fn wake_lock_waiters(&mut self) {
        self.wake_waiters(false);
    }

    /// Retries statements parked on an object read only after that read has
    /// completed. Readable client sockets do not make a pending object GET
    /// complete, so combining this with lock wakeups would spin the reactor.
    fn wake_io_waiters(&mut self) {
        self.wake_waiters(true);
    }

    fn wake_waiters(&mut self, retry_io_waiters: bool) {
        // A retry can itself abort a deadlock victim and release locks. Loop
        // until one complete pass observes a stable generation so every newly
        // unblocked connection is considered in the same reactor turn.
        for _ in 0..=self.slots.len() {
            let generation = self.engine.lock_generation();
            for index in 0..self.slots.len() {
                if !self.slots[index].conn.is_open() {
                    continue;
                }
                match self.slots[index].conn.retry_parked(
                    &mut self.engine,
                    generation,
                    retry_io_waiters,
                ) {
                    After::Continue => self.sync_write_interest(index),
                    After::Close => {
                        let slot = &mut self.slots[index];
                        self.engine.rollback_txn(&mut slot.conn.txn, &slot.conn.guc);
                        self.release(index);
                    }
                }
            }
            if self.engine.lock_generation() == generation {
                break;
            }
        }
    }

    /// Delivers every queued notification to the connections listening on its
    /// channel, then clears the outbox. A listener whose send buffer cannot hold
    /// the message (it is not draining its socket) is closed rather than sent a
    /// truncated stream.
    fn deliver_notifications(&mut self) {
        for n_index in 0..self.engine.notifications().len() {
            // `Notification` is `Copy`, so lift it out and drop the engine
            // borrow before touching the slots.
            let notification = self.engine.notifications()[n_index];
            for index in 0..self.slots.len() {
                if !self.slots[index].conn.is_open() {
                    continue;
                }
                let conn_id = self.slots[index].conn.id();
                if !self
                    .engine
                    .is_listening(conn_id, notification.channel.as_str())
                {
                    continue;
                }
                let delivered = self.slots[index].conn.queue_notification(
                    notification.pid,
                    notification.channel.as_str(),
                    notification.payload.as_str(),
                );
                if delivered {
                    self.sync_write_interest(index);
                } else {
                    self.release(index);
                }
            }
        }
        self.engine.clear_notifications();
    }

    /// Reconciles a slot's registered write interest with whether it now has
    /// buffered output (mirrors the `dispatch` tail after appending bytes out of
    /// band).
    fn sync_write_interest(&mut self, index: usize) {
        let slot = &mut self.slots[index];
        if !slot.conn.is_open() {
            return;
        }
        let read_desired = slot.conn.wants_read();
        if read_desired != slot.want_read {
            let fd = slot.conn.stream().as_raw_fd();
            let token = token_for(index as u32, slot.generation);
            match self.reactor.set_read_interest(fd, token, read_desired) {
                Ok(()) => slot.want_read = read_desired,
                Err(e) => {
                    log_io("set read interest", &e);
                    self.release(index);
                    return;
                }
            }
        }
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

    fn release(&mut self, index: usize) {
        // Drop the connection's LISTEN registrations so its channels free up and
        // no stale entry can match a later connection that reuses the id.
        self.slots[index].conn.stop_replication(&mut self.engine);
        self.engine.drop_connection(self.slots[index].conn.id());
        if let Some(role) = self.slots[index].conn.authenticated_role() {
            self.engine.release_role_connection(role);
        }
        let slot = &mut self.slots[index];
        if let Some(stream) = slot.conn.close() {
            // Closing the fd drops its kqueue registrations; an explicit
            // deregister first keeps the reactor's view tidy and catches
            // double-release bugs in debug runs.
            let _ = self.reactor.deregister(stream.as_raw_fd());
            drop(stream);
        }
        slot.generation = slot.generation.wrapping_add(1);
        slot.want_read = false;
        slot.want_write = false;
        self.free
            .push(index as u32)
            .expect("released slot cannot exceed capacity");
    }

    fn pump_replication_streams(&mut self) {
        for index in 0..self.slots.len() {
            if !self.slots[index].conn.is_open() {
                continue;
            }
            match self.slots[index].conn.pump_replication(&mut self.engine) {
                After::Continue => self.sync_write_interest(index),
                After::Close => self.release(index),
            }
        }
    }

    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }
}

/// Binds a TCP listener whose address can immediately be reused after an
/// ungraceful server exit. This is required for crash recovery: the previous
/// process may leave completed connections in TCP's closing states.
fn bind_listener(address: &str) -> std::io::Result<TcpListener> {
    let mut last_error = None;
    for socket_address in address.to_socket_addrs()? {
        match bind_socket_address(socket_address) {
            Ok(listener) => return Ok(listener),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "listen address resolved to no socket addresses",
        )
    }))
}

fn bind_socket_address(address: SocketAddr) -> std::io::Result<TcpListener> {
    let domain = match address {
        SocketAddr::V4(_) => libc::AF_INET,
        SocketAddr::V6(_) => libc::AF_INET6,
    };
    let fd = unsafe { libc::socket(domain, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let result = (|| {
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let enabled: libc::c_int = 1;
        let option_result = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                (&enabled as *const libc::c_int).cast(),
                std::mem::size_of_val(&enabled) as libc::socklen_t,
            )
        };
        if option_result != 0 {
            return Err(std::io::Error::last_os_error());
        }

        let bind_result = match address {
            SocketAddr::V4(address) => {
                #[cfg(target_os = "linux")]
                let socket_address = libc::sockaddr_in {
                    sin_family: libc::AF_INET as libc::sa_family_t,
                    sin_port: address.port().to_be(),
                    sin_addr: libc::in_addr {
                        s_addr: u32::from_ne_bytes(address.ip().octets()),
                    },
                    sin_zero: [0; 8],
                };
                #[cfg(not(target_os = "linux"))]
                let socket_address = libc::sockaddr_in {
                    sin_len: std::mem::size_of::<libc::sockaddr_in>() as u8,
                    sin_family: libc::AF_INET as u8,
                    sin_port: address.port().to_be(),
                    sin_addr: libc::in_addr {
                        s_addr: u32::from_ne_bytes(address.ip().octets()),
                    },
                    sin_zero: [0; 8],
                };
                unsafe {
                    libc::bind(
                        fd,
                        (&socket_address as *const libc::sockaddr_in).cast(),
                        std::mem::size_of_val(&socket_address) as libc::socklen_t,
                    )
                }
            }
            SocketAddr::V6(address) => {
                #[cfg(target_os = "linux")]
                let socket_address = libc::sockaddr_in6 {
                    sin6_family: libc::AF_INET6 as libc::sa_family_t,
                    sin6_port: address.port().to_be(),
                    sin6_flowinfo: address.flowinfo(),
                    sin6_addr: libc::in6_addr {
                        s6_addr: address.ip().octets(),
                    },
                    sin6_scope_id: address.scope_id(),
                };
                #[cfg(not(target_os = "linux"))]
                let socket_address = libc::sockaddr_in6 {
                    sin6_len: std::mem::size_of::<libc::sockaddr_in6>() as u8,
                    sin6_family: libc::AF_INET6 as u8,
                    sin6_port: address.port().to_be(),
                    sin6_flowinfo: address.flowinfo(),
                    sin6_addr: libc::in6_addr {
                        s6_addr: address.ip().octets(),
                    },
                    sin6_scope_id: address.scope_id(),
                };
                unsafe {
                    libc::bind(
                        fd,
                        (&socket_address as *const libc::sockaddr_in6).cast(),
                        std::mem::size_of_val(&socket_address) as libc::socklen_t,
                    )
                }
            }
        };
        if bind_result != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { libc::listen(fd, libc::SOMAXCONN) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { TcpListener::from_raw_fd(fd) })
    })();
    if result.is_err() {
        unsafe {
            libc::close(fd);
        }
    }
    result
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

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::net::TcpStream;

    use super::bind_listener;

    #[test]
    fn listener_rebinds_after_active_connection_closes() {
        let listener = bind_listener("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            let mut byte = [0; 1];
            assert_eq!(stream.read(&mut byte).unwrap(), 0);
        });
        let (stream, _) = listener.accept().unwrap();
        drop(stream);
        drop(listener);
        client.join().unwrap();

        let replacement = bind_listener(&address.to_string()).unwrap();
        assert_eq!(replacement.local_addr().unwrap(), address);
    }
}
