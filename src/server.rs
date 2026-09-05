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
use crate::pg::conn::{After, AuthContext, CancelRequest, Conn};
use crate::sql::Engine;

const LISTENER_TOKEN: u64 = u64::MAX;
const SHUTDOWN_TOKEN: u64 = u64::MAX - 1;
/// First reactor token for durable block-store in-flight GET sockets.
const BLOCK_IO_TOKEN: u64 = u64::MAX - 2;
/// First token reserved for outbound logical-subscription workers.  The
/// bounded subscription count is checked against this disjoint range at setup.
const SUBSCRIPTION_TOKEN_BASE: u64 = u64::MAX - 1_000_000;

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
    /// first 4). A matching CancelRequest interrupts a parked statement.
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
    subscriptions: FixedVec<SubscriptionWorker>,
}

struct Slot {
    conn: Conn,
    generation: u32,
    want_read: bool,
    want_write: bool,
}

struct SubscriptionWorker {
    client: crate::pg::replication_client::ReplicationClient,
    sql: crate::pg::replication_client::ReplicationClient,
    apply: crate::pg::subscription_apply::SubscriptionApply,
    bootstrap: SubscriptionBootstrapWork,
    name: Option<crate::storage::SqlName>,
    definition: Option<SubscriptionBinding>,
    registered_fd: Option<i32>,
    want_write: bool,
    retry_at: Option<std::time::Instant>,
    registered_sql_fd: Option<i32>,
    sql_want_write: bool,
    cleanup: Option<(u64, crate::storage::SqlName)>,
}

const SUBSCRIPTION_FILTER_BYTES: usize = 4096;
const SUBSCRIPTION_QUERY_BYTES: usize = 8192;

#[derive(Clone, Copy)]
struct SubscriptionBootstrapTable {
    schema: crate::storage::SqlName,
    name: crate::storage::SqlName,
    columns: [crate::storage::SqlName; crate::storage::MAX_COLUMNS],
    column_count: usize,
    filter: crate::util::StackStr<SUBSCRIPTION_FILTER_BYTES>,
    filter_all: bool,
    copy: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SubscriptionBootstrapStage {
    Idle,
    AwaitingSnapshot,
    ConnectingSql,
    Discovering,
    Copying,
    DroppingSyncSlot,
}

struct SubscriptionBootstrapWork {
    stage: SubscriptionBootstrapStage,
    snapshot: Option<crate::pg::replication_client::SlotSnapshot>,
    tables: FixedVec<SubscriptionBootstrapTable>,
    table: usize,
    copy_setup: Option<crate::sql::exec::CopySetup>,
    line: crate::mem::buffer::FixedBuf,
    binary_header_pending: bool,
    binary_end_seen: bool,
}

fn subscription_name_array(
    input: &[u8],
) -> Result<
    (
        [crate::storage::SqlName; crate::storage::MAX_COLUMNS],
        usize,
    ),
    (),
> {
    let input = core::str::from_utf8(input).map_err(|_| ())?;
    let bytes = input.as_bytes();
    if bytes.first() != Some(&b'{') || bytes.last() != Some(&b'}') {
        return Err(());
    }
    let mut names = [crate::storage::SqlName::EMPTY; crate::storage::MAX_COLUMNS];
    let mut count = 0;
    let mut at = 1;
    while at + 1 < bytes.len() {
        if count == names.len() {
            return Err(());
        }
        let mut value = crate::util::StackStr::<63>::new();
        let quoted = bytes[at] == b'"';
        if quoted {
            at += 1;
        }
        loop {
            let byte = *bytes.get(at).ok_or(())?;
            if quoted && byte == b'"' {
                at += 1;
                break;
            }
            if !quoted && matches!(byte, b',' | b'}') {
                break;
            }
            if byte == b'\\' {
                at += 1;
                let escaped = *bytes.get(at).ok_or(())?;
                use core::fmt::Write as _;
                write!(value, "{}", escaped as char).map_err(|_| ())?;
                at += 1;
                continue;
            }
            if byte == b'}' {
                return Err(());
            }
            let rest = core::str::from_utf8(&bytes[at..]).map_err(|_| ())?;
            let character = rest.chars().next().ok_or(())?;
            use core::fmt::Write as _;
            write!(value, "{character}").map_err(|_| ())?;
            at += character.len_utf8();
        }
        if value.is_truncated()
            || value.as_str().is_empty()
            || (!quoted && value.as_str() == "NULL")
        {
            return Err(());
        }
        let value = crate::storage::SqlName::parse(value.as_str()).map_err(|_| ())?;
        if names[..count].contains(&value) {
            return Err(());
        }
        names[count] = value;
        count += 1;
        match bytes.get(at) {
            Some(b',') => at += 1,
            Some(b'}') if at + 1 == bytes.len() => break,
            _ => return Err(()),
        }
    }
    if count == 0 {
        return Err(());
    }
    Ok((names, count))
}

fn append_sql_literal<const N: usize>(out: &mut crate::util::StackStr<N>, value: &str) {
    use core::fmt::Write as _;
    let _ = write!(out, "'");
    for character in value.chars() {
        if character == '\'' {
            let _ = write!(out, "''");
        } else {
            let _ = write!(out, "{character}");
        }
    }
    let _ = write!(out, "'");
}

fn append_sql_identifier<const N: usize>(out: &mut crate::util::StackStr<N>, value: &str) {
    use core::fmt::Write as _;
    let _ = write!(out, "\"");
    for character in value.chars() {
        if character == '"' {
            let _ = write!(out, "\"\"");
        } else {
            let _ = write!(out, "{character}");
        }
    }
    let _ = write!(out, "\"");
}

fn subscription_discovery_query(
    snapshot: crate::pg::replication_client::SlotSnapshot,
    publications: &[crate::storage::SqlName],
) -> Result<crate::util::StackStr<SUBSCRIPTION_QUERY_BYTES>, ()> {
    use core::fmt::Write as _;
    let mut query = crate::util::StackStr::new();
    let _ = write!(
        query,
        "BEGIN ISOLATION LEVEL REPEATABLE READ; SET TRANSACTION SNAPSHOT "
    );
    append_sql_literal(&mut query, snapshot.name.as_str());
    let _ = write!(
        query,
        "; SELECT pubname::text, schemaname::text, tablename::text, attnames::text, rowfilter FROM pg_catalog.pg_publication_tables WHERE pubname IN ("
    );
    for (index, publication) in publications.iter().enumerate() {
        if index != 0 {
            let _ = write!(query, ",");
        }
        append_sql_literal(&mut query, publication.as_str());
    }
    let _ = write!(query, ") ORDER BY schemaname, tablename, pubname");
    (!query.is_truncated()).then_some(query).ok_or(())
}

fn subscription_copy_query(
    table: SubscriptionBootstrapTable,
    binary: bool,
) -> Result<crate::util::StackStr<SUBSCRIPTION_QUERY_BYTES>, ()> {
    use core::fmt::Write as _;
    let mut query = crate::util::StackStr::new();
    let _ = write!(query, "COPY (SELECT ");
    for (index, column) in table.columns[..table.column_count].iter().enumerate() {
        if index != 0 {
            let _ = write!(query, ",");
        }
        append_sql_identifier(&mut query, column.as_str());
    }
    let _ = write!(query, " FROM ");
    append_sql_identifier(&mut query, table.schema.as_str());
    let _ = write!(query, ".");
    append_sql_identifier(&mut query, table.name.as_str());
    if !table.filter_all && !table.filter.as_str().is_empty() {
        let _ = write!(query, " WHERE {}", table.filter.as_str());
    }
    let _ = write!(query, ") TO STDOUT");
    if binary {
        let _ = write!(query, " (FORMAT binary)");
    }
    (!query.is_truncated()).then_some(query).ok_or(())
}

impl SubscriptionBootstrapWork {
    fn absorb_discovery_row(
        &mut self,
        row: crate::pg::replication_client::SqlDataRow<'_>,
    ) -> Result<(), crate::pg::replication_client::ClientError> {
        let [_, schema, table, columns, filter] = row.columns() else {
            return Err(crate::pg::replication_client::ClientError::PublisherError);
        };
        let parse_name = |value: &Option<&[u8]>| {
            core::str::from_utf8(
                value.ok_or(crate::pg::replication_client::ClientError::PublisherError)?,
            )
            .map_err(|_| crate::pg::replication_client::ClientError::PublisherError)
            .and_then(|value| {
                crate::storage::SqlName::parse(value)
                    .map_err(|_| crate::pg::replication_client::ClientError::PublisherError)
            })
        };
        let schema = parse_name(schema)?;
        let table = parse_name(table)?;
        let (columns, column_count) = subscription_name_array(
            columns.ok_or(crate::pg::replication_client::ClientError::PublisherError)?,
        )
        .map_err(|_| crate::pg::replication_client::ClientError::PublisherError)?;
        let existing = self
            .tables
            .iter()
            .position(|entry| entry.schema == schema && entry.name == table);
        let index = if let Some(index) = existing {
            let entry = self.tables[index];
            if entry.column_count != column_count
                || entry.columns[..entry.column_count] != columns[..column_count]
            {
                return Err(crate::pg::replication_client::ClientError::PublisherError);
            }
            index
        } else {
            let index = self.tables.len();
            self.tables
                .push(SubscriptionBootstrapTable {
                    schema,
                    name: table,
                    columns,
                    column_count,
                    filter: crate::util::StackStr::new(),
                    filter_all: false,
                    copy: true,
                })
                .map_err(|_| crate::pg::replication_client::ClientError::WireFull)?;
            index
        };
        let entry = &mut self.tables[index];
        match filter {
            None => {
                entry.filter_all = true;
                entry.filter = crate::util::StackStr::new();
            }
            Some(filter) if !entry.filter_all => {
                let filter = core::str::from_utf8(filter)
                    .map_err(|_| crate::pg::replication_client::ClientError::PublisherError)?;
                use core::fmt::Write as _;
                if !entry.filter.as_str().is_empty() {
                    write!(entry.filter, " OR ")
                        .map_err(|_| crate::pg::replication_client::ClientError::WireFull)?;
                }
                write!(entry.filter, "({filter})")
                    .map_err(|_| crate::pg::replication_client::ClientError::WireFull)?;
                if entry.filter.is_truncated() {
                    return Err(crate::pg::replication_client::ClientError::WireFull);
                }
            }
            Some(_) => {}
        }
        Ok(())
    }
}

/// Worker identity excludes the acknowledgement frontier: a successful apply
/// advances that frontier frequently, whereas only a committed stream
/// definition must reconnect the publisher session.
#[derive(Clone, Copy, PartialEq, Eq)]
struct SubscriptionBinding {
    stream: crate::storage::SubscriptionStream,
    endpoint: crate::pg::replication_client::ConnectionInfo,
    publications: [crate::storage::SqlName; crate::storage::MAX_SUBSCRIPTION_PUBLICATIONS],
    publication_count: usize,
    slot: Option<crate::storage::SqlName>,
    manage_slot_behavior: bool,
    bootstrap_slot: Option<crate::storage::SqlName>,
    drop_bootstrap_slot: bool,
    bootstrap: crate::storage::SubscriptionBootstrap,
    enabled: bool,
    behavior: crate::storage::SubscriptionBehavior,
}

impl From<crate::sql::SubscriptionRuntime> for SubscriptionBinding {
    fn from(runtime: crate::sql::SubscriptionRuntime) -> Self {
        Self {
            stream: runtime.stream,
            endpoint: runtime.endpoint,
            publications: runtime.publications,
            publication_count: runtime.publication_count,
            slot: runtime.slot,
            manage_slot_behavior: runtime.manage_slot_behavior,
            bootstrap_slot: runtime.bootstrap_slot,
            drop_bootstrap_slot: runtime.drop_bootstrap_slot,
            bootstrap: runtime.bootstrap,
            enabled: runtime.enabled,
            behavior: runtime.behavior,
        }
    }
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
    /// TLS-pool capacity for the complete fixed outbound subscription worker
    /// set.  The workers are allocated at startup even when their catalog
    /// entries are disabled, so enabling one cannot grow runtime memory.
    pub fn extra_tls_pool_bytes(config: &Config) -> usize {
        config.max_subscriptions * 2 * crate::object_store::tls::CLIENT_SESSION_BYTES
    }

    pub fn extra_budget_bytes(config: &Config) -> usize {
        config.max_subscriptions * core::mem::size_of::<SubscriptionWorker>()
            + config.max_subscriptions
                * (2 * crate::pg::replication_client::ReplicationClient::budget_bytes(
                    crate::storage::MAX_SUBSCRIPTION_PUBLICATIONS,
                    config.subscription_receive_bytes,
                    config.subscription_send_bytes,
                ) + crate::pg::subscription_apply::SubscriptionApply::budget_bytes(
                    config.subscription_relation_capacity,
                    config.txn_rows,
                    config.subscription_arena_bytes,
                ) + config.subscription_relation_capacity
                    * core::mem::size_of::<SubscriptionBootstrapTable>()
                    + config.copy_line_bytes)
    }

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
        let mut reactor = Reactor::new(
            budget,
            max_conns + 2 + block_read_slots + 2 * config.max_subscriptions,
        )
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
        let mut block_read_fds = FixedVec::new(budget, "block_read_fds", block_read_slots)?;
        let mut subscriptions =
            FixedVec::new(budget, "subscription_workers", config.max_subscriptions)?;
        let subscription_tls =
            crate::object_store::tls::build_client_config(&config.subscription_tls_ca_file)
                .map_err(|error| {
                    ServerSetupError::Io("subscription TLS", std::io::Error::other(error))
                })?;
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
        for _ in 0..config.max_subscriptions {
            subscriptions
                .push(SubscriptionWorker {
                    client: crate::pg::replication_client::ReplicationClient::new_unbound(
                        budget,
                        crate::storage::MAX_SUBSCRIPTION_PUBLICATIONS,
                        config.subscription_receive_bytes,
                        config.subscription_send_bytes,
                        Some(&subscription_tls),
                    )
                    .map_err(|error| {
                        ServerSetupError::Io(
                            "allocate subscription worker",
                            std::io::Error::other(error),
                        )
                    })?,
                    apply: crate::pg::subscription_apply::SubscriptionApply::new(
                        budget,
                        crate::storage::SubscriptionStream::EMPTY,
                        config.subscription_relation_capacity,
                        config.txn_rows,
                        config.subscription_arena_bytes,
                        0,
                        crate::storage::SubscriptionBehavior::POSTGRESQL_18_DEFAULT,
                    )?,
                    sql: crate::pg::replication_client::ReplicationClient::new_unbound(
                        budget,
                        0,
                        config.subscription_receive_bytes,
                        config.subscription_send_bytes,
                        Some(&subscription_tls),
                    )
                    .map_err(|error| {
                        ServerSetupError::Io(
                            "allocate subscription SQL worker",
                            std::io::Error::other(error),
                        )
                    })?,
                    bootstrap: SubscriptionBootstrapWork {
                        stage: SubscriptionBootstrapStage::Idle,
                        snapshot: None,
                        tables: FixedVec::new(
                            budget,
                            "subscription_bootstrap_tables",
                            config.subscription_relation_capacity,
                        )?,
                        table: 0,
                        copy_setup: None,
                        line: crate::mem::buffer::FixedBuf::new(
                            budget,
                            "subscription_copy_line",
                            config.copy_line_bytes,
                        )?,
                        binary_header_pending: false,
                        binary_end_seen: false,
                    },
                    name: None,
                    definition: None,
                    registered_fd: None,
                    want_write: false,
                    retry_at: None,
                    registered_sql_fd: None,
                    sql_want_write: false,
                    cleanup: None,
                })
                .expect("sized to max_subscriptions");
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
            "md5" => AuthMode::Md5,
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
            subscriptions,
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
        self.reconcile_subscriptions()?;
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
                self.next_replication_keepalive_timeout(),
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
                } else if let Some((subscription, sql)) = self.subscription_slot(event.token) {
                    self.advance_subscription(subscription, sql, event.readable, event.writable)?;
                } else {
                    self.dispatch(event.token, event.readable, event.writable);
                }
            }
            self.pump_replication_streams();
            self.reconcile_subscriptions()?;
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
        let (after, cancel_request) = if readable {
            let after = slot.conn.on_readable(
                &mut self.engine,
                &self.cancel_key,
                &self.auth,
                self.tls_config.as_ref(),
            );
            (after, slot.conn.take_cancel_request())
        } else if writable {
            (slot.conn.on_writable(), None)
        } else {
            (After::Continue, None)
        };
        if let Some(request) = cancel_request {
            self.cancel(request);
        }
        match after {
            After::Close => self.release(index),
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
        self.terminate_dropped_database_connections();
        if self.engine.take_system_settings_reload() {
            for slot in self.slots.iter() {
                if slot.conn.is_open() {
                    self.engine
                        .apply_system_settings(&slot.conn.guc)
                        .expect("stored system settings were validated before publication");
                }
            }
        }
        // A NOTIFY committed by the message just processed leaves notifications
        // in the engine outbox; fan them out to every listening connection.
        if self.engine.has_notifications() {
            self.deliver_notifications();
        }
    }

    fn terminate_dropped_database_connections(&mut self) {
        let dropped = self.engine.dropped_database_connections();
        for (database, &must_terminate) in dropped.iter().enumerate() {
            if !must_terminate {
                continue;
            }
            for index in 0..self.slots.len() {
                if !self.slots[index].conn.is_open()
                    || self.slots[index].conn.is_terminating()
                    || self.slots[index].conn.authenticated_database()
                        != u16::try_from(database).ok()
                {
                    continue;
                }
                {
                    let slot = &mut self.slots[index];
                    self.engine.rollback_txn(&mut slot.conn.txn, &slot.conn.guc);
                }
                if self.slots[index].conn.terminate_by_administrator() {
                    self.sync_write_interest(index);
                } else {
                    self.release(index);
                }
            }
        }
    }

    fn cancel(&mut self, request: CancelRequest) {
        if let Some(index) = self
            .slots
            .iter()
            .position(|slot| request.matches(slot.conn.id(), &self.cancel_key))
            && self.slots[index].conn.cancel_parked()
        {
            self.sync_write_interest(index);
        }
    }

    fn next_lock_wait_timeout(&self) -> Option<Duration> {
        self.slots
            .iter()
            .filter_map(|slot| slot.conn.lock_wait_remaining())
            .min()
    }

    fn next_replication_keepalive_timeout(&self) -> Option<Duration> {
        self.slots
            .iter()
            .filter_map(|slot| slot.conn.replication_keepalive_remaining())
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

    fn subscription_slot(&self, token: u64) -> Option<(usize, bool)> {
        let offset = token.checked_sub(SUBSCRIPTION_TOKEN_BASE)? as usize;
        let slot = offset / 2;
        (slot < self.subscriptions.len())
            .then_some(slot)
            .map(|slot| (slot, offset % 2 == 1))
    }

    fn subscription_token(slot: usize, sql: bool) -> u64 {
        SUBSCRIPTION_TOKEN_BASE + (slot * 2 + usize::from(sql)) as u64
    }

    fn unbind_subscription(&mut self, slot: usize) {
        let worker = &mut self.subscriptions[slot];
        if let Some(fd) = worker.registered_fd.take() {
            let _ = self.reactor.deregister(fd);
        }
        if let Some(fd) = worker.registered_sql_fd.take() {
            let _ = self.reactor.deregister(fd);
        }
        worker.apply.stop(&mut self.engine);
        worker.client.unbind();
        worker.sql.unbind();
        worker.apply.unbind();
        worker.bootstrap.stage = SubscriptionBootstrapStage::Idle;
        worker.bootstrap.snapshot = None;
        worker.bootstrap.tables.clear();
        worker.bootstrap.table = 0;
        worker.bootstrap.copy_setup = None;
        worker.bootstrap.line.clear();
        worker.name = None;
        worker.definition = None;
        worker.cleanup = None;
        worker.want_write = false;
        worker.sql_want_write = false;
    }

    /// Binds exactly the fixed worker matching each committed enabled catalog
    /// slot.  Failed connects use an explicit retry delay; disabled/dropped
    /// catalog state removes the reactor interest and cannot keep a live
    /// publisher socket behind it.
    fn reconcile_subscriptions(&mut self) -> std::io::Result<()> {
        let now = std::time::Instant::now();
        for slot in 0..self.subscriptions.len() {
            if let Some(cleanup) = self.engine.subscription_cleanup_runtime(slot) {
                if self.subscriptions[slot].cleanup == Some((cleanup.created_at, cleanup.name)) {
                    self.sync_subscription_interest(slot)?;
                    continue;
                }
                if self.subscriptions[slot]
                    .retry_at
                    .is_some_and(|deadline| deadline > now)
                {
                    continue;
                }
                if self.subscriptions[slot].name.is_some() {
                    self.unbind_subscription(slot);
                }
                let worker = &mut self.subscriptions[slot];
                match worker.client.bind_drop_slot(cleanup.endpoint, cleanup.slot) {
                    Ok(()) => {
                        worker.name = Some(cleanup.name);
                        worker.cleanup = Some((cleanup.created_at, cleanup.name));
                        worker.retry_at = None;
                        let fd = worker.client.raw_fd();
                        self.reactor
                            .register_read(fd, Self::subscription_token(slot, false))?;
                        worker.registered_fd = Some(fd);
                        self.sync_subscription_interest(slot)?;
                    }
                    Err(_) => {
                        worker.retry_at = Some(now + Duration::from_secs(1));
                    }
                }
                continue;
            }
            if self.subscriptions[slot].cleanup.is_some() {
                self.unbind_subscription(slot);
            }
            let runtime = self.engine.subscription_runtime(slot);
            let bound = self.subscriptions[slot].definition;
            match (runtime, bound) {
                (None, Some(_)) => self.unbind_subscription(slot),
                (None, None) => {}
                (Some(runtime), Some(binding)) if binding == runtime.into() => {
                    self.sync_subscription_interest(slot)?;
                }
                (Some(runtime), _) => {
                    if self.subscriptions[slot]
                        .retry_at
                        .is_some_and(|deadline| deadline > now)
                    {
                        continue;
                    }
                    if self.subscriptions[slot].name.is_some() {
                        self.unbind_subscription(slot);
                    }
                    let worker = &mut self.subscriptions[slot];
                    worker
                        .apply
                        .bind(runtime.stream, runtime.confirmed_lsn, runtime.behavior)
                        .map_err(|_| std::io::Error::other("bind subscription apply"))?;
                    let bind = match runtime.bootstrap {
                        crate::storage::SubscriptionBootstrap::CreateManagedSlot { copy_data } => {
                            let bootstrap_slot = runtime.bootstrap_slot.ok_or_else(|| {
                                std::io::Error::other(
                                    "managed subscription bootstrap has no publisher slot",
                                )
                            })?;
                            worker.bootstrap.stage = SubscriptionBootstrapStage::AwaitingSnapshot;
                            worker.bootstrap.snapshot = None;
                            worker.bootstrap.tables.clear();
                            worker.bootstrap.table = 0;
                            worker.bootstrap.copy_setup = None;
                            worker.bootstrap.line.clear();
                            let result = worker.client.bind_create_slot(
                                runtime.endpoint,
                                bootstrap_slot,
                                runtime.behavior,
                            );
                            if result.is_err() {
                                worker.bootstrap.stage = SubscriptionBootstrapStage::Idle;
                            }
                            let _ = copy_data;
                            result
                        }
                        crate::storage::SubscriptionBootstrap::CopyExternalSlot
                        | crate::storage::SubscriptionBootstrap::CopyWithoutSlot
                        | crate::storage::SubscriptionBootstrap::Refresh { .. } => {
                            let bootstrap_slot = runtime.bootstrap_slot.ok_or_else(|| {
                                std::io::Error::other(
                                    "subscription synchronization has no temporary slot",
                                )
                            })?;
                            worker.bootstrap.stage = SubscriptionBootstrapStage::AwaitingSnapshot;
                            worker.bootstrap.snapshot = None;
                            worker.bootstrap.tables.clear();
                            worker.bootstrap.table = 0;
                            worker.bootstrap.copy_setup = None;
                            worker.bootstrap.line.clear();
                            let result = worker.client.bind_create_slot(
                                runtime.endpoint,
                                bootstrap_slot,
                                crate::storage::SubscriptionBehavior::POSTGRESQL_18_DEFAULT,
                            );
                            if result.is_err() {
                                worker.bootstrap.stage = SubscriptionBootstrapStage::Idle;
                            }
                            result
                        }
                        crate::storage::SubscriptionBootstrap::Ready if runtime.enabled => {
                            let publisher_slot = runtime.slot.ok_or_else(|| {
                                std::io::Error::other("enabled subscription has no publisher slot")
                            })?;
                            worker.client.bind(
                                crate::pg::replication_client::ReplicationClientSetup {
                                    endpoint: runtime.endpoint,
                                    slot: publisher_slot,
                                    publications: &runtime.publications
                                        [..runtime.publication_count],
                                    start_lsn: runtime.confirmed_lsn,
                                    protocol: crate::pg::pgoutput::ProtocolVersion::V4,
                                    behavior: runtime.behavior,
                                    manage_slot_behavior: runtime.manage_slot_behavior,
                                },
                            )
                        }
                        _ => {
                            worker.apply.unbind();
                            continue;
                        }
                    };
                    match bind {
                        Ok(()) => {
                            worker.name = Some(runtime.stream.name());
                            worker.definition = Some(runtime.into());
                            worker.retry_at = None;
                            let fd = worker.client.raw_fd();
                            self.reactor
                                .register_read(fd, Self::subscription_token(slot, false))?;
                            worker.registered_fd = Some(fd);
                            self.sync_subscription_interest(slot)?;
                        }
                        Err(_) => {
                            worker.apply.unbind();
                            worker.client.unbind();
                            worker.retry_at = Some(now + Duration::from_secs(1));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn sync_subscription_interest(&mut self, slot: usize) -> std::io::Result<()> {
        let worker = &mut self.subscriptions[slot];
        let Some(fd) = worker.registered_fd else {
            return Ok(());
        };
        let wanted = worker.client.wants_write();
        if wanted != worker.want_write {
            self.reactor
                .set_write_interest(fd, Self::subscription_token(slot, false), wanted)?;
            worker.want_write = wanted;
        }
        Ok(())
    }

    fn sync_subscription_sql_interest(&mut self, slot: usize) -> std::io::Result<()> {
        let worker = &mut self.subscriptions[slot];
        let Some(fd) = worker.registered_sql_fd else {
            return Ok(());
        };
        let wanted = worker.sql.wants_write();
        if wanted != worker.sql_want_write {
            self.reactor
                .set_write_interest(fd, Self::subscription_token(slot, true), wanted)?;
            worker.sql_want_write = wanted;
        }
        Ok(())
    }

    fn queue_subscription_copy(&mut self, slot: usize) -> Result<bool, crate::sql::eval::SqlError> {
        let worker = &mut self.subscriptions[slot];
        while worker.bootstrap.table < worker.bootstrap.tables.len()
            && !worker.bootstrap.tables[worker.bootstrap.table].copy
        {
            worker.bootstrap.table += 1;
        }
        if worker.bootstrap.table == worker.bootstrap.tables.len() {
            return Ok(false);
        }
        let table = *worker
            .bootstrap
            .tables
            .get(worker.bootstrap.table)
            .ok_or_else(|| {
                crate::sql_err!(
                    crate::sql::eval::sqlstate::INTERNAL_ERROR,
                    "subscription COPY table cursor is invalid"
                )
            })?;
        let setup = worker.apply.start_copy_table(
            &mut self.engine,
            table.schema,
            table.name,
            &table.columns[..table.column_count],
        )?;
        let binary = worker
            .definition
            .expect("bootstrap worker has a definition")
            .behavior
            .binary;
        let query = subscription_copy_query(table, binary).map_err(|_| {
            crate::sql_err!(
                crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "subscription COPY query exceeds its fixed capacity"
            )
        })?;
        worker.sql.query(query.as_str()).map_err(|_| {
            crate::sql_err!(
                crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "subscription publisher send buffer is full"
            )
        })?;
        worker.bootstrap.copy_setup = Some(setup);
        worker.bootstrap.binary_header_pending = binary;
        worker.bootstrap.binary_end_seen = false;
        worker.bootstrap.stage = SubscriptionBootstrapStage::Copying;
        Ok(true)
    }

    fn finish_or_drop_subscription_bootstrap(
        &mut self,
        slot: usize,
        copied_tables: bool,
    ) -> Result<bool, ()> {
        let worker = &mut self.subscriptions[slot];
        let binding = worker.definition.ok_or(())?;
        if binding.drop_bootstrap_slot {
            if let Some(fd) = worker.registered_fd.take() {
                let _ = self.reactor.deregister(fd);
            }
            worker.client.unbind();
            worker
                .client
                .bind_drop_slot(binding.endpoint, binding.bootstrap_slot.ok_or(())?)
                .map_err(|_| ())?;
            let fd = worker.client.raw_fd();
            self.reactor
                .register_read(fd, Self::subscription_token(slot, false))
                .map_err(|_| ())?;
            worker.registered_fd = Some(fd);
            worker.want_write = false;
            worker.bootstrap.stage = SubscriptionBootstrapStage::DroppingSyncSlot;
            return Ok(false);
        }
        let snapshot = worker.bootstrap.snapshot.ok_or(())?;
        let result = if copied_tables {
            worker
                .apply
                .finish_bootstrap(&mut self.engine, snapshot.consistent_lsn)
        } else {
            worker
                .apply
                .establish_frontier(&mut self.engine, snapshot.consistent_lsn)
        };
        result.map_err(|_| ())?;
        self.unbind_subscription(slot);
        Ok(true)
    }

    fn advance_subscription(
        &mut self,
        slot: usize,
        sql: bool,
        readable: bool,
        writable: bool,
    ) -> std::io::Result<()> {
        if sql {
            return self.advance_subscription_sql(slot, readable, writable);
        }
        let worker = &mut self.subscriptions[slot];
        let mut failed = false;
        let mut cleanup_slot_absent = false;
        let mut publisher_failure = None;
        if writable && worker.client.writable().is_err() {
            failed = true;
        }
        if !failed && readable {
            let mut acknowledgement = None;
            let readable = worker.client.readable(|event| {
                let crate::pg::replication_client::ClientEvent::Replication(frame) = event else {
                    return Ok(());
                };
                match worker.apply.receive(&mut self.engine, frame) {
                    Ok(crate::pg::subscription_apply::ApplyResult::None) => Ok(()),
                    Ok(crate::pg::subscription_apply::ApplyResult::Acknowledge {
                        flushed_lsn,
                        reply_requested,
                    }) => {
                        acknowledgement = Some((flushed_lsn, reply_requested));
                        Ok(())
                    }
                    Err(error) => {
                        log_subscription_error(worker.name, &error);
                        Err(crate::pg::replication_client::ClientError::PublisherError)
                    }
                }
            });
            match readable {
                Ok(()) => {}
                Err(crate::pg::replication_client::ClientError::Publisher(error))
                    if worker.cleanup.is_some()
                        && error.sqlstate == crate::sql::eval::sqlstate::UNDEFINED_OBJECT =>
                {
                    // DROP is driven only by a durable managed-slot cleanup
                    // intent. If a crash happened after the remote side effect
                    // but before its local completion record, absence proves
                    // that the requested state has already been reached.
                    cleanup_slot_absent = true;
                }
                Err(error) => {
                    log_subscription_client_error(worker.name, &error);
                    if let crate::pg::replication_client::ClientError::Publisher(error) = error {
                        publisher_failure = Some(error);
                    }
                    failed = true;
                }
            }
            if let Some((flushed_lsn, reply_requested)) = acknowledgement
                && worker
                    .client
                    .acknowledge(flushed_lsn, reply_requested)
                    .is_err()
            {
                failed = true;
            }
            if !failed
                && (worker.client.command_complete() || cleanup_slot_absent)
                && let Some((created_at, name)) = worker.cleanup
            {
                if self
                    .engine
                    .complete_subscription_cleanup(slot, created_at, name)
                    .is_err()
                {
                    failed = true;
                } else {
                    self.unbind_subscription(slot);
                    return Ok(());
                }
            }
            if !failed
                && worker.bootstrap.stage == SubscriptionBootstrapStage::DroppingSyncSlot
                && worker.client.command_complete()
            {
                let snapshot = worker
                    .bootstrap
                    .snapshot
                    .expect("sync-slot drop retains snapshot frontier");
                let completed = worker
                    .apply
                    .finish_bootstrap(&mut self.engine, snapshot.consistent_lsn);
                if completed.is_err() {
                    failed = true;
                } else {
                    self.unbind_subscription(slot);
                    return Ok(());
                }
            }
            if !failed
                && worker.bootstrap.stage == SubscriptionBootstrapStage::AwaitingSnapshot
                && let Some(snapshot) = worker.client.slot_snapshot()
            {
                let endpoint = worker
                    .definition
                    .expect("bound subscription has a definition")
                    .endpoint;
                match worker.sql.bind_sql(endpoint) {
                    Ok(()) => {
                        let fd = worker.sql.raw_fd();
                        if self
                            .reactor
                            .register_read(fd, Self::subscription_token(slot, true))
                            .is_err()
                        {
                            failed = true;
                        } else {
                            worker.registered_sql_fd = Some(fd);
                            worker.bootstrap.snapshot = Some(snapshot);
                            worker.bootstrap.stage = SubscriptionBootstrapStage::ConnectingSql;
                        }
                    }
                    Err(_) => failed = true,
                }
            }
        }
        if failed {
            let binding = self.subscriptions[slot].definition;
            let stream = binding.map(|binding| binding.stream);
            let retry = binding.is_none_or(|binding| !binding.behavior.disable_on_error);
            self.unbind_subscription(slot);
            if let (Some(stream), Some(failure)) = (stream, publisher_failure) {
                let failure = crate::storage::SubscriptionFailure {
                    sqlstate: failure.sqlstate,
                    message: failure.message,
                };
                if let Err(error) = self.engine.fail_subscription(stream, failure) {
                    log_subscription_error(Some(stream.name()), &error);
                }
            }
            if retry {
                self.subscriptions[slot].retry_at =
                    Some(std::time::Instant::now() + Duration::from_secs(1));
            }
        } else {
            self.sync_subscription_interest(slot)?;
            self.sync_subscription_sql_interest(slot)?;
        }
        Ok(())
    }

    fn advance_subscription_sql(
        &mut self,
        slot: usize,
        readable: bool,
        writable: bool,
    ) -> std::io::Result<()> {
        let worker = &mut self.subscriptions[slot];
        let mut failed = writable && worker.sql.writable().is_err();
        let mut publisher_failure = None;
        let mut local_failure = None;
        let mut connected = false;
        let mut discovery_ready = false;
        let mut table_ready = false;
        if !failed && readable {
            let stage = worker.bootstrap.stage;
            let result = worker.sql.readable(|event| {
                let crate::pg::replication_client::ClientEvent::Sql(event) = event else {
                    return Err(crate::pg::replication_client::ClientError::PublisherError);
                };
                match (stage, event) {
                    (
                        SubscriptionBootstrapStage::ConnectingSql,
                        crate::pg::replication_client::SqlEvent::Ready {
                            transaction_status: b'I',
                        },
                    ) => connected = true,
                    (
                        SubscriptionBootstrapStage::Discovering,
                        crate::pg::replication_client::SqlEvent::RowDescription { fields: 5 },
                    ) => {}
                    (
                        SubscriptionBootstrapStage::Discovering,
                        crate::pg::replication_client::SqlEvent::DataRow(row),
                    ) => worker.bootstrap.absorb_discovery_row(row)?,
                    (
                        SubscriptionBootstrapStage::Discovering,
                        crate::pg::replication_client::SqlEvent::CommandComplete { .. },
                    ) => {}
                    (
                        SubscriptionBootstrapStage::Discovering,
                        crate::pg::replication_client::SqlEvent::Ready {
                            transaction_status: b'T',
                        },
                    ) => discovery_ready = true,
                    (
                        SubscriptionBootstrapStage::Copying,
                        crate::pg::replication_client::SqlEvent::CopyOut {
                            fields,
                            binary,
                        },
                    ) if usize::from(fields)
                        == worker
                            .bootstrap
                            .copy_setup
                            .expect("copying stage owns setup")
                            .n_targets
                        && binary
                            == worker
                                .definition
                                .expect("copying stage owns definition")
                                .behavior
                                .binary => {}
                    (
                        SubscriptionBootstrapStage::Copying,
                        crate::pg::replication_client::SqlEvent::CopyData(bytes),
                    ) => {
                        let binary = worker
                            .definition
                            .expect("copying stage owns definition")
                            .behavior
                            .binary;
                        if binary {
                            if !worker.bootstrap.line.append(bytes) {
                                return Err(crate::pg::replication_client::ClientError::WireFull);
                            }
                            if worker.bootstrap.binary_header_pending {
                                match crate::sql::copy::binary_header(
                                    worker.bootstrap.line.readable(),
                                ) {
                                    crate::sql::copy::BinaryHeader::Incomplete => return Ok(()),
                                    crate::sql::copy::BinaryHeader::Bad => {
                                        return Err(crate::pg::replication_client::ClientError::PublisherError);
                                    }
                                    crate::sql::copy::BinaryHeader::Done(length) => {
                                        worker.bootstrap.line.consume(length);
                                        worker.bootstrap.binary_header_pending = false;
                                    }
                                }
                            }
                            loop {
                                match crate::sql::copy::binary_frame(
                                    worker.bootstrap.line.readable(),
                                ) {
                                    crate::sql::copy::BinaryFrame::Incomplete => break,
                                    crate::sql::copy::BinaryFrame::Bad => {
                                        return Err(crate::pg::replication_client::ClientError::PublisherError);
                                    }
                                    crate::sql::copy::BinaryFrame::Trailer => {
                                        worker.bootstrap.line.consume(2);
                                        worker.bootstrap.binary_end_seen = true;
                                    }
                                    crate::sql::copy::BinaryFrame::Row(length) => {
                                        if worker.bootstrap.binary_end_seen {
                                            return Err(crate::pg::replication_client::ClientError::PublisherError);
                                        }
                                        let setup = worker
                                            .bootstrap
                                            .copy_setup
                                            .expect("copying stage owns setup");
                                        if let Err(error) = worker.apply.copy_binary_row(
                                            &mut self.engine,
                                            &setup,
                                            &worker.bootstrap.line.readable()[..length],
                                        ) {
                                            local_failure = Some(error);
                                            return Err(crate::pg::replication_client::ClientError::PublisherError);
                                        }
                                        worker.bootstrap.line.consume(length);
                                    }
                                }
                            }
                        } else {
                            for byte in bytes {
                                if *byte == b'\n' {
                                    let setup = worker
                                        .bootstrap
                                        .copy_setup
                                        .expect("copying stage owns setup");
                                    if let Err(error) = worker.apply.copy_line(
                                        &mut self.engine,
                                        &setup,
                                        worker.bootstrap.line.readable(),
                                    ) {
                                        local_failure = Some(error);
                                        return Err(crate::pg::replication_client::ClientError::PublisherError);
                                    }
                                    worker.bootstrap.line.clear();
                                } else if !worker.bootstrap.line.append(&[*byte]) {
                                    return Err(crate::pg::replication_client::ClientError::WireFull);
                                }
                            }
                        }
                    }
                    (
                        SubscriptionBootstrapStage::Copying,
                        crate::pg::replication_client::SqlEvent::CopyDone,
                    ) => {
                        let binary = worker
                            .definition
                            .expect("copying stage owns definition")
                            .behavior
                            .binary;
                        if !worker.bootstrap.line.is_empty()
                            || (binary
                                && (worker.bootstrap.binary_header_pending
                                    || !worker.bootstrap.binary_end_seen))
                        {
                            return Err(crate::pg::replication_client::ClientError::PublisherError);
                        }
                        let setup = worker
                            .bootstrap
                            .copy_setup
                            .expect("copying stage owns setup");
                        if let Err(error) = worker.apply.finish_copy_table(&mut self.engine, &setup)
                        {
                            local_failure = Some(error);
                            return Err(crate::pg::replication_client::ClientError::PublisherError);
                        }
                    }
                    (
                        SubscriptionBootstrapStage::Copying,
                        crate::pg::replication_client::SqlEvent::CommandComplete { .. },
                    ) => {}
                    (
                        SubscriptionBootstrapStage::Copying,
                        crate::pg::replication_client::SqlEvent::Ready {
                            transaction_status: b'T',
                        },
                    ) => table_ready = true,
                    _ => {
                        return Err(crate::pg::replication_client::ClientError::PublisherError);
                    }
                }
                Ok(())
            });
            if let Err(error) = &result {
                if let Some(local) = &local_failure {
                    log_subscription_error(worker.name, local);
                } else {
                    log_subscription_client_error(worker.name, error);
                }
                if let crate::pg::replication_client::ClientError::Publisher(error) = error {
                    publisher_failure = Some(*error);
                }
            }
            failed = result.is_err();
        }
        if !failed && connected {
            let binding = worker.definition.expect("bound bootstrap definition");
            let snapshot = worker.bootstrap.snapshot.expect("created slot snapshot");
            let query = subscription_discovery_query(
                snapshot,
                &binding.publications[..binding.publication_count],
            );
            match query.and_then(|query| worker.sql.query(query.as_str()).map_err(|_| ())) {
                Ok(()) => worker.bootstrap.stage = SubscriptionBootstrapStage::Discovering,
                Err(()) => failed = true,
            }
        }
        if !failed && discovery_ready {
            let copy_data = worker.definition.is_some_and(|binding| {
                matches!(
                    binding.bootstrap,
                    crate::storage::SubscriptionBootstrap::CreateManagedSlot { copy_data: true }
                        | crate::storage::SubscriptionBootstrap::CopyExternalSlot
                        | crate::storage::SubscriptionBootstrap::CopyWithoutSlot
                        | crate::storage::SubscriptionBootstrap::Refresh { copy_data: true }
                )
            });
            let stream = worker
                .definition
                .expect("bound bootstrap definition")
                .stream;
            for table in worker.bootstrap.tables.iter_mut() {
                table.copy = copy_data
                    && !self.engine.subscription_relation_is_ready(
                        stream,
                        table.schema.as_str(),
                        table.name.as_str(),
                    );
            }
            let has_copy = worker.bootstrap.tables.iter().any(|table| table.copy);
            if let Err(error) = worker.apply.begin_bootstrap(&mut self.engine) {
                local_failure = Some(error);
                failed = true;
            } else {
                for table in worker.bootstrap.tables.iter() {
                    if let Err(error) = worker.apply.register_bootstrap_relation(
                        &mut self.engine,
                        table.schema,
                        table.name,
                    ) {
                        local_failure = Some(error);
                        failed = true;
                        break;
                    }
                }
            }
            if !failed && !has_copy {
                match self.finish_or_drop_subscription_bootstrap(slot, true) {
                    Ok(true) => return Ok(()),
                    Ok(false) => {}
                    Err(()) => failed = true,
                }
            } else if !failed {
                match self.queue_subscription_copy(slot) {
                    Ok(true) => {}
                    Ok(false) => failed = true,
                    Err(error) => {
                        local_failure = Some(error);
                        failed = true;
                    }
                }
            }
        }
        if !failed && table_ready {
            let worker = &mut self.subscriptions[slot];
            worker.bootstrap.table += 1;
            worker.bootstrap.copy_setup = None;
            match self.queue_subscription_copy(slot) {
                Ok(false) => match self.finish_or_drop_subscription_bootstrap(slot, true) {
                    Ok(true) => return Ok(()),
                    Ok(false) => {}
                    Err(()) => failed = true,
                },
                Ok(true) => {}
                Err(error) => {
                    local_failure = Some(error);
                    failed = true;
                }
            }
        }
        if failed {
            let binding = self.subscriptions[slot].definition;
            let stream = binding.map(|binding| binding.stream);
            let retry = binding.is_none_or(|binding| !binding.behavior.disable_on_error);
            if let Some(failure) = &local_failure {
                log_subscription_error(self.subscriptions[slot].name, failure);
            }
            self.unbind_subscription(slot);
            let durable_failure = local_failure
                .map(|failure| crate::storage::SubscriptionFailure {
                    sqlstate: failure.sqlstate,
                    message: failure.message,
                })
                .or_else(|| {
                    publisher_failure.map(|failure| crate::storage::SubscriptionFailure {
                        sqlstate: failure.sqlstate,
                        message: failure.message,
                    })
                });
            if let (Some(stream), Some(failure)) = (stream, durable_failure)
                && let Err(error) = self.engine.fail_subscription(stream, failure)
            {
                log_subscription_error(Some(stream.name()), &error);
            }
            if retry {
                self.subscriptions[slot].retry_at =
                    Some(std::time::Instant::now() + Duration::from_secs(1));
            }
        } else {
            self.sync_subscription_interest(slot)?;
            self.sync_subscription_sql_interest(slot)?;
        }
        Ok(())
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
                    After::Close => self.release(index),
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
        // Every transport exit reaches this choke point. Roll back here so an
        // I/O-interest failure, notification overflow, or replication close
        // cannot strand transaction state or locks.
        let slot = &mut self.slots[index];
        self.engine.rollback_txn(&mut slot.conn.txn, &slot.conn.guc);
        self.slots[index].conn.stop_replication(&mut self.engine);
        self.engine.drop_connection(self.slots[index].conn.id());
        if let Some(role) = self.slots[index].conn.authenticated_role() {
            self.engine.release_role_connection(role);
        }
        if let Some(database) = self.slots[index].conn.authenticated_database() {
            self.engine.release_database_connection(database);
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
    use core::fmt::Write as _;
    let mut message = crate::util::StackStr::<256>::new();
    let _ = writeln!(
        message,
        "pos3ql: {context}: kind={:?} os_error={:?}",
        e.kind(),
        e.raw_os_error()
    );
    stderr_line(message.as_str().as_bytes());
}

fn log_subscription_error(
    name: Option<crate::storage::SqlName>,
    error: &crate::sql::eval::SqlError,
) {
    use core::fmt::Write as _;
    let mut message = crate::util::StackStr::<512>::new();
    let _ = writeln!(
        message,
        "pos3ql: subscription {} apply failed [{}]: {}",
        name.as_ref().map_or("<unknown>", |name| name.as_str()),
        error.sqlstate,
        error.message.as_str()
    );
    stderr_line(message.as_str().as_bytes());
}

fn log_subscription_client_error(
    name: Option<crate::storage::SqlName>,
    error: &crate::pg::replication_client::ClientError,
) {
    use core::fmt::Write as _;
    let mut message = crate::util::StackStr::<512>::new();
    match error {
        crate::pg::replication_client::ClientError::Publisher(error) => {
            let _ = writeln!(
                message,
                "pos3ql: subscription {} publisher failed [{}]: {}",
                name.as_ref().map_or("<unknown>", |name| name.as_str()),
                error.sqlstate,
                error.message.as_str()
            );
        }
        crate::pg::replication_client::ClientError::Io(error) => {
            let _ = writeln!(
                message,
                "pos3ql: subscription {} transport failed: kind={:?} os_error={:?}",
                name.as_ref().map_or("<unknown>", |name| name.as_str()),
                error.kind(),
                error.raw_os_error()
            );
        }
        _ => {
            let _ = writeln!(
                message,
                "pos3ql: subscription {} protocol failed: {:?}",
                name.as_ref().map_or("<unknown>", |name| name.as_str()),
                error
            );
        }
    }
    stderr_line(message.as_str().as_bytes());
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::net::TcpStream;

    use super::{
        SubscriptionBinding, SubscriptionBootstrapStage, SubscriptionBootstrapWork, bind_listener,
    };

    fn discovery_row<'a>(
        publication: &'a [u8],
        schema: &'a [u8],
        table: &'a [u8],
        columns: &'a [u8],
        filter: Option<&'a [u8]>,
    ) -> crate::pg::replication_client::SqlDataRow<'a> {
        crate::pg::replication_client::SqlDataRow::for_test(&[
            Some(publication),
            Some(schema),
            Some(table),
            Some(columns),
            filter,
        ])
    }

    fn bootstrap_work(budget: &mut crate::mem::budget::Budget) -> SubscriptionBootstrapWork {
        SubscriptionBootstrapWork {
            stage: SubscriptionBootstrapStage::Idle,
            snapshot: None,
            tables: crate::mem::fixed_vec::FixedVec::new(budget, "test_subscription_tables", 2)
                .unwrap(),
            table: 0,
            copy_setup: None,
            line: crate::mem::buffer::FixedBuf::new(budget, "test_subscription_copy", 256).unwrap(),
            binary_header_pending: false,
            binary_end_seen: false,
        }
    }

    #[test]
    fn subscription_bootstrap_requires_matching_columns_and_ors_filters() {
        let mut budget = crate::mem::budget::Budget::new(1 << 20);
        let mut work = bootstrap_work(&mut budget);
        work.absorb_discovery_row(discovery_row(
            b"left_changes",
            b"public",
            b"items",
            b"{id,left_value}",
            Some(b"id > 0"),
        ))
        .unwrap();
        work.absorb_discovery_row(discovery_row(
            b"right_changes",
            b"public",
            b"items",
            b"{id,left_value}",
            Some(b"id < 0"),
        ))
        .unwrap();

        assert_eq!(work.tables.len(), 1);
        let table = work.tables[0];
        assert_eq!(table.column_count, 2);
        assert_eq!(table.columns[0].as_str(), "id");
        assert_eq!(table.columns[1].as_str(), "left_value");
        assert_eq!(table.filter.as_str(), "(id > 0) OR (id < 0)");
        assert!(!table.filter_all);

        work.absorb_discovery_row(discovery_row(
            b"all_rows",
            b"public",
            b"items",
            b"{id,left_value}",
            None,
        ))
        .unwrap();
        let table = work.tables[0];
        assert!(table.filter_all);
        assert!(table.filter.as_str().is_empty());
    }

    #[test]
    fn subscription_bootstrap_rejects_duplicate_remote_columns() {
        let mut budget = crate::mem::budget::Budget::new(1 << 20);
        let mut work = bootstrap_work(&mut budget);
        assert!(
            work.absorb_discovery_row(discovery_row(
                b"changes", b"public", b"items", b"{id,id}", None,
            ))
            .is_err()
        );
    }

    #[test]
    fn subscription_bootstrap_rejects_different_publication_column_lists() {
        let mut budget = crate::mem::budget::Budget::new(1 << 20);
        let mut work = bootstrap_work(&mut budget);
        work.absorb_discovery_row(discovery_row(
            b"left_changes",
            b"public",
            b"items",
            b"{id,left_value}",
            None,
        ))
        .unwrap();
        assert!(
            work.absorb_discovery_row(discovery_row(
                b"right_changes",
                b"public",
                b"items",
                b"{id,right_value}",
                None,
            ))
            .is_err()
        );
    }

    #[test]
    fn subscription_name_array_rejects_duplicate_remote_columns() {
        assert!(super::subscription_name_array(b"{id,id}").is_err());
        assert!(super::subscription_name_array(b"{id,\"id\"}").is_err());
    }

    #[test]
    fn subscription_binding_reconnects_only_for_stream_definition_changes() {
        let endpoint = crate::pg::replication_client::ConnectionInfo::parse(
            "host=127.0.0.1 port=5432 user=repl dbname=publisher application_name=apply sslmode=disable",
        )
        .unwrap();
        let mut publications =
            [crate::storage::SqlName::EMPTY; crate::storage::MAX_SUBSCRIPTION_PUBLICATIONS];
        publications[0] = crate::storage::SqlName::parse("sales").unwrap();
        let runtime = crate::sql::SubscriptionRuntime {
            stream: crate::storage::SubscriptionStream::for_test(
                crate::storage::SqlName::parse("apply").unwrap(),
                7,
            ),
            endpoint,
            publications,
            publication_count: 1,
            slot: Some(crate::storage::SqlName::parse("publisher_slot").unwrap()),
            manage_slot_behavior: false,
            bootstrap_slot: Some(crate::storage::SqlName::parse("publisher_slot").unwrap()),
            drop_bootstrap_slot: false,
            confirmed_lsn: 12,
            bootstrap: crate::storage::SubscriptionBootstrap::Ready,
            enabled: true,
            behavior: crate::storage::SubscriptionBehavior::POSTGRESQL_18_DEFAULT,
        };
        let binding = SubscriptionBinding::from(runtime);
        let mut advanced = runtime;
        advanced.confirmed_lsn = 13;
        assert!(binding == SubscriptionBinding::from(advanced));
        let mut replaced = runtime;
        replaced.stream = crate::storage::SubscriptionStream::for_test(
            crate::storage::SqlName::parse("apply").unwrap(),
            8,
        );
        assert!(binding != SubscriptionBinding::from(replaced));
        let mut altered = runtime;
        altered.publications[0] = crate::storage::SqlName::parse("inventory").unwrap();
        assert!(binding != SubscriptionBinding::from(altered));
    }

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
