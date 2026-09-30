//! The single-threaded server: one reactor, a fixed array of connection
//! slots whose buffers are allocated once at startup, and the query engine.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::fd::{AsRawFd, FromRawFd};
use std::time::Duration;

use crate::config::Config;
use crate::io::reactor::Reactor;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::pg::auth::{AuthMode, SCRAM_ITERATIONS, ScramServer};
use crate::pg::conn::{After, AuthContext, CancelRequest, Conn, PendingResponse};
use crate::sql::{Engine, ExecutionIdentity, QueryWorkspaceId};

const LISTENER_TOKEN: u64 = u64::MAX;
const SHUTDOWN_TOKEN: u64 = u64::MAX - 1;
/// First reactor token for durable block-store in-flight GET sockets.
const BLOCK_IO_TOKEN: u64 = u64::MAX - 2;
/// First token reserved for outbound logical-subscription workers.  The
/// bounded subscription count is checked against this disjoint range at setup.
const SUBSCRIPTION_TOKEN_BASE: u64 = u64::MAX - 1_000_000;
const OPERATIONS_LISTENER_TOKEN: u64 = u64::MAX - 2_000_000;
const OPERATIONS_TOKEN_BASE: u64 = 1 << 63;
const OPERATIONS_REQUEST_BYTES: usize = 2048;
const OPERATIONS_RESPONSE_BYTES: usize = 16 * 1024;

/// Set by the signal handler; the loop drains and exits when it sees this.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
static RELOAD_REQUESTED: AtomicBool = AtomicBool::new(false);
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

extern "C" fn on_reload_signal(_sig: libc::c_int) {
    RELOAD_REQUESTED.store(true, Ordering::SeqCst);
    let fd = SHUTDOWN_PIPE_WRITE.load(Ordering::SeqCst);
    if fd >= 0 {
        let byte = [2u8];
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
    query_workspaces: QueryWorkspaceLeases,
    query_dispatches: QueryDispatchQueue,
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
    operations_listener: Option<TcpListener>,
    operations_slots: FixedVec<OperationsSlot>,
    operations_free: FixedVec<u32>,
    operations_metrics: OperationsMetrics,
    capacity_limits: CapacityLimits,
    memory_reserved_bytes: usize,
    durability_ready: bool,
    ownership_ready: bool,
    credentials_ready: bool,
    credentials_file: crate::util::StackStr<{ crate::object_store::CREDENTIAL_FILE_BYTES }>,
}

struct Slot {
    conn: Conn,
    generation: u32,
    want_read: bool,
    want_write: bool,
    query_dispatch_pending: bool,
    pending_response: Option<PendingQueryResponse>,
}

/// Engine work returned to the reactor with the exact lease and session
/// identity that produced it.
struct QueryDispatchCompletion {
    owner: usize,
    generation: u32,
    workspace: QueryWorkspaceId,
    identity: ExecutionIdentity,
    response: PendingResponse,
    cancel_request: Option<CancelRequest>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct QueryDispatch {
    owner: usize,
    generation: u32,
    workspace: QueryWorkspaceId,
    kind: QueryDispatchKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueryDispatchKind {
    Readable,
    Retry {
        lock_generation: u64,
        retry_io_waiters: bool,
    },
}

/// Startup-bounded FIFO between reactor readiness and engine execution.
struct QueryDispatchQueue {
    slots: FixedVec<Option<QueryDispatch>>,
    head: usize,
    len: usize,
}

impl QueryDispatchQueue {
    fn new(budget: &mut Budget, capacity: usize) -> Result<Self, BudgetError> {
        let mut slots = FixedVec::new(budget, "query_dispatch_queue", capacity)?;
        for _ in 0..capacity {
            slots.push(None).expect("sized to query_workspace_slots");
        }
        Ok(Self {
            slots,
            head: 0,
            len: 0,
        })
    }

    const fn budget_bytes(capacity: usize) -> usize {
        capacity * core::mem::size_of::<Option<QueryDispatch>>()
    }

    fn push(&mut self, dispatch: QueryDispatch) {
        assert!(
            self.len < self.slots.len(),
            "query dispatch queue exhausted despite exclusive workspace leases"
        );
        let tail = (self.head + self.len) % self.slots.len();
        assert!(self.slots[tail].replace(dispatch).is_none());
        self.len += 1;
    }

    fn pop(&mut self) -> Option<QueryDispatch> {
        if self.len == 0 {
            return None;
        }
        let dispatch = self.slots[self.head]
            .take()
            .expect("occupied query dispatch queue slot");
        self.head = (self.head + 1) % self.slots.len();
        self.len -= 1;
        Some(dispatch)
    }

    fn len(&self) -> usize {
        self.len
    }
}

/// A response waiting at the shared publication barrier after its statement
/// workspace has been released.
struct PendingQueryResponse {
    response: PendingResponse,
    identity: ExecutionIdentity,
}

/// Exclusive ownership of the engine's startup-reserved query workspaces.
/// The waiting roster is bounded by the connection-slot count and preserves
/// arrival order when dispatch workers eventually outlive a reactor turn.
struct QueryWorkspaceLeases {
    owners: FixedVec<Option<usize>>,
    waiters: FixedVec<usize>,
    next: usize,
}

impl QueryWorkspaceLeases {
    fn new(
        budget: &mut Budget,
        workspace_count: usize,
        connection_count: usize,
    ) -> Result<Self, BudgetError> {
        let mut owners = FixedVec::new(budget, "query_workspace_owners", workspace_count)?;
        for _ in 0..workspace_count {
            owners.push(None).expect("sized to query_workspace_slots");
        }
        Ok(Self {
            owners,
            waiters: FixedVec::new(budget, "query_workspace_waiters", connection_count)?,
            next: 0,
        })
    }

    const fn budget_bytes(workspace_count: usize, connection_count: usize) -> usize {
        workspace_count * core::mem::size_of::<Option<usize>>()
            + connection_count * core::mem::size_of::<usize>()
    }

    fn acquire(&mut self, owner: usize) -> Option<QueryWorkspaceId> {
        if let Some(workspace) = self.try_acquire(owner) {
            return Some(workspace);
        }
        if !self.waiters.contains(&owner) {
            self.waiters
                .push(owner)
                .expect("one waiter per fixed connection slot");
        }
        None
    }

    fn try_acquire(&mut self, owner: usize) -> Option<QueryWorkspaceId> {
        if let Some(index) = self
            .owners
            .iter()
            .position(|candidate| *candidate == Some(owner))
        {
            return Some(QueryWorkspaceId::from_index(index));
        }
        for offset in 0..self.owners.len() {
            let index = (self.next + offset) % self.owners.len();
            if self.owners[index].is_none() {
                self.owners[index] = Some(owner);
                self.next = (index + 1) % self.owners.len();
                if let Some(waiter) = self.waiters.iter().position(|waiter| *waiter == owner) {
                    self.remove_waiter(waiter);
                }
                return Some(QueryWorkspaceId::from_index(index));
            }
        }
        None
    }

    /// Releases `owner` and hands the same workspace to the oldest waiter.
    fn release(&mut self, owner: usize) -> Option<(usize, QueryWorkspaceId)> {
        let workspace = self
            .owners
            .iter()
            .position(|candidate| *candidate == Some(owner))?;
        self.owners[workspace] = None;
        if self.waiters.is_empty() {
            return None;
        }
        let next_owner = self.waiters[0];
        self.remove_waiter(0);
        self.owners[workspace] = Some(next_owner);
        Some((next_owner, QueryWorkspaceId::from_index(workspace)))
    }

    fn release_workspace(
        &mut self,
        owner: usize,
        workspace: QueryWorkspaceId,
    ) -> Option<(usize, QueryWorkspaceId)> {
        assert_eq!(
            self.owners[workspace.index()],
            Some(owner),
            "query completion must release its exact workspace lease"
        );
        self.release(owner)
    }

    fn cancel(&mut self, owner: usize) -> Option<(usize, QueryWorkspaceId)> {
        if let Some(waiter) = self.waiters.iter().position(|waiter| *waiter == owner) {
            self.remove_waiter(waiter);
        }
        self.release(owner)
    }

    fn remove_waiter(&mut self, index: usize) {
        for position in index + 1..self.waiters.len() {
            self.waiters[position - 1] = self.waiters[position];
        }
        self.waiters.pop();
    }

    fn used(&self) -> usize {
        self.owners.iter().filter(|owner| owner.is_some()).count()
    }

    fn waiting(&self) -> usize {
        self.waiters.len()
    }
}

struct OperationsSlot {
    stream: Option<TcpStream>,
    request: crate::mem::buffer::FixedBuf,
    response: crate::mem::buffer::FixedBuf,
}

#[derive(Default)]
struct OperationsMetrics {
    postgres_accepted: u64,
    postgres_refused: u64,
    postgres_closed: u64,
    http_requests: u64,
    http_errors: u64,
    credential_reload_successes: u64,
    credential_reload_failures: u64,
}

#[derive(Clone, Copy)]
struct CapacityLimits {
    postgres_connections: usize,
    query_workspace_slots: usize,
    operations_connections: usize,
    block_cache_bytes: usize,
    disk_cache_bytes: usize,
    temporary_spill_bytes: usize,
    tables: usize,
    indexes: usize,
    databases: usize,
    schemas: usize,
    roles: usize,
    prepared_transactions: usize,
    replication_slots: usize,
    subscriptions: usize,
    foreign_sessions: usize,
    object_store: bool,
    credential_rotation: bool,
    tls_budget_bytes: usize,
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
    endpoint: crate::pg::replication_client::SubscriptionEndpoint,
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
    fn block_read_slots(config: &Config) -> usize {
        let plan =
            crate::store::StackPlan::resolve(config.block_cache_bytes, config.disk_cache_bytes);
        if config.object_store_on && (plan.ram.units > 0 || plan.disk.units > 0) {
            config.object_store_get_slots
        } else {
            0
        }
    }

    /// Bytes reserved directly by the server around the separately budgeted
    /// per-connection buffers and engine storage.
    pub fn budget_bytes(config: &Config) -> usize {
        let connections = config.max_connections as usize;
        let operations_connections = if config.operations_listen_addr.is_empty() {
            0
        } else {
            config.operations_max_connections
        };
        let block_reads = Self::block_read_slots(config);
        Reactor::budget_bytes(
            connections
                + 2
                + block_reads
                + 2 * config.max_subscriptions
                + usize::from(operations_connections != 0)
                + operations_connections,
        ) + 128
            + connections * (core::mem::size_of::<Slot>() + core::mem::size_of::<u32>())
            + QueryWorkspaceLeases::budget_bytes(config.query_workspace_slots, connections)
            + QueryDispatchQueue::budget_bytes(config.query_workspace_slots)
            + operations_connections
                * (core::mem::size_of::<OperationsSlot>()
                    + core::mem::size_of::<u32>()
                    + OPERATIONS_REQUEST_BYTES
                    + OPERATIONS_RESPONSE_BYTES)
            + block_reads * core::mem::size_of::<Option<i32>>()
            + Self::extra_budget_bytes(config)
    }

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
                ) + crate::pg::subscription_apply::SubscriptionApply::budget_bytes(config)
                    + config.subscription_relation_capacity
                        * core::mem::size_of::<SubscriptionBootstrapTable>()
                    + config.copy_line_bytes)
    }

    pub fn new(config: &Config, budget: &mut Budget) -> Result<Self, ServerSetupError> {
        let max_conns = config.max_connections as usize;
        let operations_connections = if config.operations_listen_addr.is_empty() {
            0
        } else {
            config.operations_max_connections
        };
        let listener = bind_listener(&config.listen_addr)
            .map_err(|e| ServerSetupError::Io("bind listen_addr", e))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| ServerSetupError::Io("set listener nonblocking", e))?;
        let operations_listener = if config.operations_listen_addr.is_empty() {
            None
        } else {
            let listener = bind_listener(&config.operations_listen_addr)
                .map_err(|e| ServerSetupError::Io("bind operations_listen_addr", e))?;
            listener
                .set_nonblocking(true)
                .map_err(|e| ServerSetupError::Io("set operations listener nonblocking", e))?;
            Some(listener)
        };

        let block_read_slots = Self::block_read_slots(config);
        #[allow(
            unused_mut,
            reason = "the Linux epoll backend records fixed read/write interest"
        )]
        let mut reactor = Reactor::new(
            budget,
            max_conns
                + 2
                + block_read_slots
                + 2 * config.max_subscriptions
                + usize::from(operations_listener.is_some())
                + operations_connections,
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
        if let Some(listener) = &operations_listener {
            reactor
                .register_read(listener.as_raw_fd(), OPERATIONS_LISTENER_TOKEN)
                .map_err(|e| ServerSetupError::Io("register operations listener", e))?;
        }

        let mut slots = FixedVec::new(budget, "conn_slots", max_conns)?;
        let mut free = FixedVec::new(budget, "conn_free_list", max_conns)?;
        let query_workspaces =
            QueryWorkspaceLeases::new(budget, config.query_workspace_slots, max_conns)?;
        let query_dispatches = QueryDispatchQueue::new(budget, config.query_workspace_slots)?;
        let mut block_read_fds = FixedVec::new(budget, "block_read_fds", block_read_slots)?;
        let mut subscriptions =
            FixedVec::new(budget, "subscription_workers", config.max_subscriptions)?;
        let mut operations_slots =
            FixedVec::new(budget, "operations_slots", operations_connections)?;
        let mut operations_free =
            FixedVec::new(budget, "operations_free_list", operations_connections)?;
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
                    query_dispatch_pending: false,
                    pending_response: None,
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
                        config,
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
        for index in (0..operations_connections as u32).rev() {
            operations_slots
                .push(OperationsSlot {
                    stream: None,
                    request: crate::mem::buffer::FixedBuf::new(
                        budget,
                        "operations_request",
                        OPERATIONS_REQUEST_BYTES,
                    )?,
                    response: crate::mem::buffer::FixedBuf::new(
                        budget,
                        "operations_response",
                        OPERATIONS_RESPONSE_BYTES,
                    )?,
                })
                .expect("sized to operations_max_connections");
            operations_free
                .push(index)
                .expect("sized to operations_max_connections");
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
        SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
        RELOAD_REQUESTED.store(false, Ordering::SeqCst);
        // Install handlers for SIGTERM, SIGINT, and credential reload.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_signal as *const () as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
            libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
            sa.sa_sigaction = on_reload_signal as *const () as usize;
            libc::sigaction(libc::SIGHUP, &sa, std::ptr::null_mut());
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

        let memory_reserved_bytes = budget.total();
        Ok(Self {
            reactor,
            listener,
            slots,
            free,
            query_workspaces,
            query_dispatches,
            engine,
            cancel_key,
            next_conn_id: 1,
            refusal,
            auth,
            tls_config,
            shutdown_read: pipe_fds[0],
            block_read_fds,
            subscriptions,
            operations_listener,
            operations_slots,
            operations_free,
            operations_metrics: OperationsMetrics::default(),
            capacity_limits: CapacityLimits {
                postgres_connections: max_conns,
                query_workspace_slots: config.query_workspace_slots,
                operations_connections,
                block_cache_bytes: config.block_cache_bytes,
                disk_cache_bytes: config.disk_cache_bytes,
                temporary_spill_bytes: config.temporary_spill_bytes,
                tables: config.max_tables,
                indexes: config.max_indexes,
                databases: config.max_databases,
                schemas: config.max_schemas,
                roles: config.max_roles,
                prepared_transactions: config.max_prepared_transactions,
                replication_slots: config.max_replication_slots,
                subscriptions: config.max_subscriptions,
                foreign_sessions: config.max_foreign_sessions,
                object_store: config.object_store_on,
                credential_rotation: !config.object_store_credentials_file.is_empty(),
                tls_budget_bytes: config.tls_pool_bytes
                    + Self::extra_tls_pool_bytes(config)
                    + if config.tls_on {
                        config.max_connections as usize * crate::pg::tls::SERVER_SESSION_BYTES
                    } else {
                        0
                    },
            },
            memory_reserved_bytes,
            durability_ready: true,
            ownership_ready: true,
            credentials_ready: true,
            credentials_file: crate::util::StackStr::from_str(
                &config.object_store_credentials_file,
            ),
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
                self.next_termination_timeout(),
                hedge_timeout,
            ]
            .into_iter()
            .flatten()
            .min();
            let n = self.reactor.poll(timeout)?;
            // Retry any journal publication left by a failed prior turn once,
            // before another statement can observe the local-only commit.
            // All readable connections in this poll then share the one
            // publication barrier at the end of the turn.
            let retrying_durable_publication = self.engine.durable_retry_pending();
            let publication_ready = match self.engine.commit_wal() {
                Ok(()) => {
                    if retrying_durable_publication {
                        self.durability_ready = true;
                    }
                    true
                }
                Err(_) => {
                    self.durability_ready = false;
                    false
                }
            };
            let mut completed_block_read = false;
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
                } else if event.token == OPERATIONS_LISTENER_TOKEN {
                    self.accept_operations_pending();
                } else if let Some(slot) = self.block_slot(event.token) {
                    completed_block_read |= self.advance_block_io(slot)?;
                } else if let Some((subscription, sql)) = self.subscription_slot(event.token) {
                    if publication_ready {
                        self.advance_subscription(
                            subscription,
                            sql,
                            event.readable,
                            event.writable,
                        )?;
                    }
                } else if let Some(slot) = self.operations_slot(event.token) {
                    self.dispatch_operations(slot, event.readable, event.writable);
                } else {
                    self.dispatch(
                        event.token,
                        event.readable,
                        event.writable,
                        publication_ready,
                    );
                }
            }
            self.drain_query_dispatches();
            if publication_ready {
                if completed_block_read {
                    self.wake_io_waiters();
                }
                // A lock timeout can be the event that woke the reactor, with
                // no socket readiness and no lock-generation change.
                self.wake_lock_waiters();
            }
            let (had_responses, responses_durable) = self.finish_response_batch();
            if publication_ready && responses_durable {
                self.process_committed_side_effects();
                self.pump_replication_streams();
                self.reconcile_subscriptions()?;
            }
            if RELOAD_REQUESTED.swap(false, Ordering::SeqCst)
                || self.engine.take_object_store_credentials_reload()
            {
                self.reload_object_store_credentials();
            }
            self.close_expired_terminations();
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
            // regardless of traffic. Work is paced unless critical cache
            // pressure requires publication before dispatch resumes; bucket
            // errors back off.
            if self.engine.checkpoint_work_pending() || (had_responses && responses_durable) {
                let checkpoint_was_pending = self.engine.checkpoint_work_pending();
                beat_backoff = if self.engine.maybe_checkpoint() {
                    if checkpoint_was_pending {
                        self.durability_ready = true;
                    }
                    Duration::ZERO
                } else {
                    self.durability_ready = false;
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
            b"shutdown requested, draining
",
        );
        let _ = self.reactor.deregister(self.listener.as_raw_fd());
        if let Some(listener) = &self.operations_listener {
            let _ = self.reactor.deregister(listener.as_raw_fd());
        }
        for index in 0..self.operations_slots.len() {
            if self.operations_slots[index].stream.is_some() {
                self.release_operations(index);
            }
        }
        for i in 0..self.slots.len() {
            if self.slots[i].conn.is_open() {
                self.release(i);
            }
        }
        if self.engine.checkpoint_enabled() {
            match self.engine.checkpoint() {
                Ok(true) => stderr_line(
                    b"final checkpoint written
",
                ),
                Ok(false) => {}
                Err(_) => stderr_line(
                    b"final checkpoint failed; journal is durable
",
                ),
            }
        }
        // Ensure the journal is durable even if no checkpoint ran.
        if self.engine.commit_wal().is_err() {
            stderr_line(b"final WAL upload failed\n");
        } else if self.engine.mark_clean_shutdown().is_err() {
            stderr_line(b"clean shutdown marker failed\n");
        }
        stderr_line(
            b"shutdown complete
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
            self.operations_metrics.postgres_refused =
                self.operations_metrics.postgres_refused.saturating_add(1);
            // Best-effort refusal; the startup response is small enough
            // that a fresh socket buffer will take it without blocking.
            use std::io::Write;
            let mut s = stream;
            let (bytes, n) = &self.refusal;
            let _ = s.write(&bytes[..*n]);
            return;
        };
        self.operations_metrics.postgres_accepted =
            self.operations_metrics.postgres_accepted.saturating_add(1);
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

    fn dispatch(&mut self, token: u64, readable: bool, writable: bool, publication_ready: bool) {
        let index = (token & 0xffff_ffff) as usize;
        let generation = (token >> 32) as u32;
        if index >= self.slots.len() {
            return;
        }
        let slot = &self.slots[index];
        if slot.generation != generation || !slot.conn.is_open() {
            // Stale event for a slot that was already recycled.
            return;
        }
        if slot.query_dispatch_pending {
            return;
        }
        if readable && !slot.want_read {
            // Read interest was removed when this slot entered the dispatch
            // queue or workspace wait roster. Ignore an already-returned event.
            return;
        }
        if readable && !publication_ready {
            // Match the per-connection barrier's prior behavior: no command
            // is allowed to run behind an unpublished local commit.
            self.complete_dispatch(index, After::Close);
            return;
        }
        if readable {
            let workspace = match self.query_workspaces.acquire(index) {
                Some(workspace) => workspace,
                None if self.query_dispatches.len() != 0 => {
                    // Drain the queued scheduler chunk, but leave every response
                    // buffered for the one end-of-turn publication barrier.
                    self.drain_query_dispatches();
                    let Some(workspace) = self.query_workspaces.acquire(index) else {
                        self.park_for_query_workspace(index);
                        return;
                    };
                    workspace
                }
                None => {
                    self.park_for_query_workspace(index);
                    return;
                }
            };
            if !self.suspend_query_read(index, "queue query dispatch") {
                return;
            }
            self.enqueue_query_dispatch(QueryDispatch {
                owner: index,
                generation,
                workspace,
                kind: QueryDispatchKind::Readable,
            });
            return;
        }
        let slot = &mut self.slots[index];
        let after = if writable {
            slot.conn.on_writable()
        } else {
            After::Continue
        };
        self.complete_dispatch(index, after);
    }

    fn park_for_query_workspace(&mut self, index: usize) {
        let _ = self.suspend_query_read(index, "park for query workspace");
    }

    fn suspend_query_read(&mut self, index: usize, context: &'static str) -> bool {
        let slot = &mut self.slots[index];
        if !slot.want_read {
            return false;
        }
        let fd = slot.conn.stream().as_raw_fd();
        let token = token_for(index as u32, slot.generation);
        match self.reactor.set_read_interest(fd, token, false) {
            Ok(()) => {
                slot.want_read = false;
                true
            }
            Err(error) => {
                log_io(context, &error);
                self.release(index);
                false
            }
        }
    }

    fn enqueue_query_dispatch(&mut self, dispatch: QueryDispatch) {
        let slot = &mut self.slots[dispatch.owner];
        assert_eq!(slot.generation, dispatch.generation);
        assert!(!slot.query_dispatch_pending);
        slot.query_dispatch_pending = true;
        self.query_dispatches.push(dispatch);
    }

    fn drain_query_dispatches(&mut self) {
        while let Some(dispatch) = self.query_dispatches.pop() {
            assert!(dispatch.owner < self.slots.len());
            assert_eq!(
                self.slots[dispatch.owner].generation, dispatch.generation,
                "queued query dispatch cannot outlive its connection generation"
            );
            assert!(self.slots[dispatch.owner].conn.is_open());
            assert!(self.slots[dispatch.owner].query_dispatch_pending);
            self.engine.select_query_workspace(dispatch.workspace);
            let response = match dispatch.kind {
                QueryDispatchKind::Readable => Some(self.slots[dispatch.owner].conn.on_readable(
                    &mut self.engine,
                    &self.cancel_key,
                    &self.auth,
                    self.tls_config.as_ref(),
                )),
                QueryDispatchKind::Retry {
                    lock_generation,
                    retry_io_waiters,
                } => self.slots[dispatch.owner].conn.retry_parked(
                    &mut self.engine,
                    lock_generation,
                    retry_io_waiters,
                ),
            };
            let Some(response) = response else {
                self.slots[dispatch.owner].query_dispatch_pending = false;
                self.release_query_workspace(dispatch.owner, dispatch.workspace);
                continue;
            };
            let completion = QueryDispatchCompletion {
                owner: dispatch.owner,
                generation: dispatch.generation,
                workspace: dispatch.workspace,
                identity: self.engine.execution_identity(),
                response,
                cancel_request: self.slots[dispatch.owner].conn.take_cancel_request(),
            };
            self.complete_query_execution(completion);
        }
    }

    fn release_query_workspace(&mut self, owner: usize, workspace: QueryWorkspaceId) {
        let handoff = self.query_workspaces.release_workspace(owner, workspace);
        self.wake_query_workspace_handoff(handoff);
    }

    /// Accepts one completed engine dispatch. The workspace becomes reusable
    /// here, while the response retains only the session identity required by
    /// later publication and transport completion.
    fn complete_query_execution(&mut self, completion: QueryDispatchCompletion) {
        assert!(self.slots[completion.owner].query_dispatch_pending);
        self.slots[completion.owner].query_dispatch_pending = false;
        self.release_query_workspace(completion.owner, completion.workspace);
        if self.slots[completion.owner].generation != completion.generation
            || !self.slots[completion.owner].conn.is_open()
        {
            return;
        }
        let after = if completion.response.closes() {
            // Session teardown is visible immediately: a later event in this
            // reactor turn must not observe temporary objects or locks owned
            // by a client that already sent Terminate/EOF.
            self.engine.restore_execution_identity(completion.identity);
            Some(
                self.slots[completion.owner]
                    .conn
                    .finish_response(completion.response, None),
            )
        } else {
            self.slots[completion.owner].pending_response = Some(PendingQueryResponse {
                response: completion.response,
                identity: completion.identity,
            });
            None
        };
        if let Some(request) = completion.cancel_request {
            self.cancel(request);
        }
        if let Some(after) = after {
            self.complete_dispatch(completion.owner, after);
        }
    }

    fn wake_query_workspace_handoff(&mut self, mut handoff: Option<(usize, QueryWorkspaceId)>) {
        while let Some((index, _workspace)) = handoff {
            if index >= self.slots.len() || !self.slots[index].conn.is_open() {
                handoff = self.query_workspaces.cancel(index);
                continue;
            }
            if !self.slots[index].conn.wants_read() {
                handoff = self.query_workspaces.cancel(index);
                continue;
            }
            let slot = &mut self.slots[index];
            if slot.want_read {
                return;
            }
            let fd = slot.conn.stream().as_raw_fd();
            let token = token_for(index as u32, slot.generation);
            match self.reactor.set_read_interest(fd, token, true) {
                Ok(()) => {
                    slot.want_read = true;
                    return;
                }
                Err(error) => {
                    log_io("resume query workspace waiter", &error);
                    self.release(index);
                    return;
                }
            }
        }
    }

    /// Publishes all journal records produced during one reactor turn, then
    /// releases every buffered response. The fixed slot array is also the
    /// fixed-capacity group-commit queue.
    fn finish_response_batch(&mut self) -> (bool, bool) {
        let had_responses = self
            .slots
            .iter()
            .any(|slot| slot.pending_response.is_some());
        if !had_responses {
            return (false, true);
        }
        let retrying_durable_publication = self.engine.durable_retry_pending();
        let publication_error = self.engine.commit_wal().err();
        if publication_error.is_some() {
            self.durability_ready = false;
        } else if retrying_durable_publication {
            self.durability_ready = true;
        }
        for index in 0..self.slots.len() {
            let Some(pending) = self.slots[index].pending_response.take() else {
                continue;
            };
            self.engine.restore_execution_identity(pending.identity);
            let after = self.slots[index]
                .conn
                .finish_response(pending.response, publication_error.as_ref());
            self.complete_dispatch(index, after);
        }
        (true, publication_error.is_none())
    }

    fn complete_dispatch(&mut self, index: usize, after: After) {
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
    }

    fn process_committed_side_effects(&mut self) {
        self.process_backend_signals();
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
        for database in 0..self.engine.database_connection_capacity() {
            if !self.engine.database_connection_must_terminate(database) {
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
                self.engine
                    .restore_execution_identity(self.slots[index].conn.execution_identity());
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
        let Some(index) = self
            .slots
            .iter()
            .position(|slot| request.matches(slot.conn.id(), &self.cancel_key))
        else {
            return;
        };
        self.engine
            .restore_execution_identity(self.slots[index].conn.execution_identity());
        if self.slots[index].conn.cancel_parked(&mut self.engine) {
            self.sync_write_interest(index);
        }
    }

    fn process_backend_signals(&mut self) {
        while let Some(signal) = self.engine.take_backend_signal() {
            let (pid, terminate) = match signal {
                crate::storage::BackendSignal::Cancel(pid) => (pid, false),
                crate::storage::BackendSignal::Terminate(pid) => (pid, true),
            };
            let Some(index) = self
                .slots
                .iter()
                .position(|slot| slot.conn.is_open() && slot.conn.id() == pid)
            else {
                continue;
            };
            self.engine
                .restore_execution_identity(self.slots[index].conn.execution_identity());
            if terminate {
                {
                    let slot = &mut self.slots[index];
                    self.engine.rollback_txn(&mut slot.conn.txn, &slot.conn.guc);
                }
                if self.slots[index].conn.terminate_by_administrator() {
                    self.sync_write_interest(index);
                } else {
                    self.release(index);
                }
            } else if self.slots[index].conn.cancel_parked(&mut self.engine) {
                self.sync_write_interest(index);
            }
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

    fn next_termination_timeout(&self) -> Option<Duration> {
        self.slots
            .iter()
            .filter_map(|slot| slot.conn.termination_remaining())
            .min()
    }

    fn close_expired_terminations(&mut self) {
        for index in 0..self.slots.len() {
            if self.slots[index].conn.is_open() && self.slots[index].conn.termination_expired() {
                self.release(index);
            }
        }
    }

    /// Called when the block-store client's non-blocking GET socket is
    /// readable. Advances the pending response read; if it completes, the
    /// block is now cached and any parked statement is retried.
    fn advance_block_io(&mut self, slot: usize) -> std::io::Result<bool> {
        let completed = self
            .engine
            .advance_pending_block_read(slot)
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::Other))?;
        // Wakeups run after all reactor events, so resumed statements join the
        // same fixed-capacity response/publication batch as readable clients.
        Ok(completed)
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
                match worker
                    .client
                    .bind_drop_slot(cleanup.endpoint.connection(), cleanup.slot)
                {
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
                                runtime.endpoint.connection(),
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
                                runtime.endpoint.connection(),
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
                                    endpoint: runtime.endpoint.connection(),
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
                .bind_drop_slot(
                    binding.endpoint.connection(),
                    binding.bootstrap_slot.ok_or(())?,
                )
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
                match worker.sql.bind_sql(endpoint.connection()) {
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
                if !self.slots[index].conn.is_open() || self.slots[index].pending_response.is_some()
                {
                    continue;
                }
                let workspace = match self.query_workspaces.try_acquire(index) {
                    Some(workspace) => workspace,
                    None if self.query_dispatches.len() != 0 => {
                        self.drain_query_dispatches();
                        let Some(workspace) = self.query_workspaces.try_acquire(index) else {
                            continue;
                        };
                        workspace
                    }
                    None => continue,
                };
                self.enqueue_query_dispatch(QueryDispatch {
                    owner: index,
                    generation: self.slots[index].generation,
                    workspace,
                    kind: QueryDispatchKind::Retry {
                        lock_generation: generation,
                        retry_io_waiters,
                    },
                });
            }
            self.drain_query_dispatches();
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
        assert!(
            !self.slots[index].query_dispatch_pending,
            "connection release must wait for its queued query dispatch"
        );
        self.engine
            .restore_execution_identity(self.slots[index].conn.execution_identity());
        let workspace_handoff = self.query_workspaces.cancel(index);
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
        slot.query_dispatch_pending = false;
        slot.pending_response = None;
        self.free
            .push(index as u32)
            .expect("released slot cannot exceed capacity");
        self.operations_metrics.postgres_closed =
            self.operations_metrics.postgres_closed.saturating_add(1);
        self.wake_query_workspace_handoff(workspace_handoff);
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

    fn accept_operations_pending(&mut self) {
        loop {
            let accepted = {
                let Some(listener) = &self.operations_listener else {
                    return;
                };
                listener.accept()
            };
            match accepted {
                Ok((stream, _)) => self.admit_operations(stream),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => {
                    log_io("accept operational connection", &error);
                    return;
                }
            }
        }
    }

    fn admit_operations(&mut self, mut stream: TcpStream) {
        if let Err(error) = stream.set_nonblocking(true) {
            log_io("set operational connection nonblocking", &error);
            return;
        }
        let _ = stream.set_nodelay(true);
        let Some(index) = self.operations_free.pop() else {
            let _ = stream.write(
                b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            );
            self.operations_metrics.http_errors =
                self.operations_metrics.http_errors.saturating_add(1);
            return;
        };
        let fd = stream.as_raw_fd();
        let slot = &mut self.operations_slots[index as usize];
        slot.request.clear();
        slot.response.clear();
        slot.stream = Some(stream);
        if let Err(error) = self
            .reactor
            .register_read(fd, Self::operations_token(index as usize))
        {
            log_io("register operational connection", &error);
            self.release_operations(index as usize);
        }
    }

    fn dispatch_operations(&mut self, index: usize, readable: bool, writable: bool) {
        if index >= self.operations_slots.len() || self.operations_slots[index].stream.is_none() {
            return;
        }
        if readable && self.operations_slots[index].response.is_empty() {
            self.read_operations(index);
        }
        if writable
            && index < self.operations_slots.len()
            && self.operations_slots[index].stream.is_some()
            && !self.operations_slots[index].response.is_empty()
        {
            self.write_operations(index);
        }
    }

    fn read_operations(&mut self, index: usize) {
        let read = {
            let slot = &mut self.operations_slots[index];
            if slot.request.writable().is_empty() {
                self.queue_operations_error(index, 431, "request headers exceed 2048 bytes\n");
                return;
            }
            let stream = slot.stream.as_mut().expect("open operational slot");
            stream.read(slot.request.writable())
        };
        match read {
            Ok(0) => self.release_operations(index),
            Ok(bytes) => {
                self.operations_slots[index].request.advance(bytes);
                match parse_operations_request(self.operations_slots[index].request.readable()) {
                    Ok(Some(endpoint)) => self.answer_operations(index, endpoint),
                    Ok(None) => {
                        if self.operations_slots[index].request.len()
                            == self.operations_slots[index].request.capacity()
                        {
                            self.queue_operations_error(
                                index,
                                431,
                                "request headers exceed 2048 bytes\n",
                            );
                        }
                    }
                    Err(HttpRequestError::Method) => {
                        self.queue_operations_error(index, 405, "only GET is supported\n")
                    }
                    Err(HttpRequestError::Target) => {
                        self.queue_operations_error(index, 404, "unknown operational endpoint\n")
                    }
                    Err(HttpRequestError::Syntax) => {
                        self.queue_operations_error(index, 400, "malformed HTTP request\n")
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                log_io("read operational request", &error);
                self.release_operations(index);
            }
        }
    }

    fn answer_operations(&mut self, index: usize, endpoint: OperationsEndpoint) {
        self.operations_metrics.http_requests =
            self.operations_metrics.http_requests.saturating_add(1);
        match endpoint {
            OperationsEndpoint::Live => {
                self.queue_operations_response(
                    index,
                    200,
                    "application/json",
                    "{\"status\":\"live\"}\n",
                );
            }
            OperationsEndpoint::Ready => {
                self.refresh_writer_readiness();
                if self.ready() {
                    self.queue_operations_response(
                        index,
                        200,
                        "application/json",
                        "{\"status\":\"ready\"}\n",
                    );
                } else {
                    self.operations_metrics.http_errors =
                        self.operations_metrics.http_errors.saturating_add(1);
                    let body = if !self.credentials_ready {
                        "{\"status\":\"not_ready\",\"reason\":\"object_store_credentials_unavailable\"}\n"
                    } else {
                        "{\"status\":\"not_ready\",\"reason\":\"durable_progress_unavailable\"}\n"
                    };
                    self.queue_operations_response(index, 503, "application/json", body);
                }
            }
            OperationsEndpoint::Metrics => {
                self.refresh_writer_readiness();
                let mut body = crate::util::StackStr::<12000>::new();
                self.render_metrics(&mut body);
                if body.is_truncated() {
                    self.queue_operations_error(index, 500, "metrics response exceeded capacity\n");
                } else {
                    self.queue_operations_response(
                        index,
                        200,
                        "text/plain; version=0.0.4; charset=utf-8",
                        body.as_str(),
                    );
                }
            }
            OperationsEndpoint::Capacity => {
                let mut body = crate::util::StackStr::<4096>::new();
                self.render_capacity(&mut body);
                if body.is_truncated() {
                    self.queue_operations_error(
                        index,
                        500,
                        "capacity response exceeded capacity\n",
                    );
                } else {
                    self.queue_operations_response(index, 200, "application/json", body.as_str());
                }
            }
        }
    }

    fn ready(&self) -> bool {
        self.durability_ready && self.ownership_ready && self.credentials_ready
    }

    fn reload_object_store_credentials(&mut self) {
        if self.credentials_file.is_empty() {
            return;
        }
        if self.engine.object_store_reads_busy() {
            RELOAD_REQUESTED.store(true, Ordering::SeqCst);
            return;
        }
        let credentials =
            match crate::object_store::load_credentials_file(self.credentials_file.as_str()) {
                Ok(credentials) => credentials,
                Err(error) => {
                    self.credentials_ready = false;
                    self.operations_metrics.credential_reload_failures = self
                        .operations_metrics
                        .credential_reload_failures
                        .saturating_add(1);
                    crate::logging::error_args(
                        "object_store_credentials_reload_failed",
                        format_args!("{error}"),
                    );
                    return;
                }
            };
        match self.engine.rotate_object_store_credentials(credentials) {
            Ok(()) => {
                self.credentials_ready = true;
                self.ownership_ready = true;
                self.operations_metrics.credential_reload_successes = self
                    .operations_metrics
                    .credential_reload_successes
                    .saturating_add(1);
                crate::logging::info(
                    "object_store_credentials_reloaded",
                    "object-store credentials validated and installed",
                );
            }
            Err(_) => {
                self.credentials_ready = false;
                self.operations_metrics.credential_reload_failures = self
                    .operations_metrics
                    .credential_reload_failures
                    .saturating_add(1);
                crate::logging::error(
                    "object_store_credentials_reload_failed",
                    "candidate failed writer-fence validation; installed credentials retained",
                );
            }
        }
    }

    fn refresh_writer_readiness(&mut self) {
        self.ownership_ready = self.engine.verify_writer_ownership().is_ok();
    }

    fn render_metrics(&self, out: &mut crate::util::StackStr<12000>) {
        use core::fmt::Write as _;
        let snapshot = self.engine.operational_snapshot();
        let postgres_active = self.capacity_limits.postgres_connections - self.free.len();
        let operations_active =
            self.capacity_limits.operations_connections - self.operations_free.len();
        let ready = u8::from(self.ready());
        let pending = u8::from(snapshot.checkpoint_pending);
        let io = snapshot.block_io;
        let _ = write!(
            out,
            "# HELP pos3ql_up Whether the process event loop is serving requests.\n\
# TYPE pos3ql_up gauge\n\
pos3ql_up 1\n\
# HELP pos3ql_ready Whether the process can acknowledge durable work.\n\
# TYPE pos3ql_ready gauge\n\
pos3ql_ready {ready}\n\
# TYPE pos3ql_lsn gauge\n\
pos3ql_lsn {}\n\
# TYPE pos3ql_postgres_connections gauge\n\
pos3ql_postgres_connections {postgres_active}\n\
# TYPE pos3ql_postgres_connection_capacity gauge\n\
pos3ql_postgres_connection_capacity {}\n\
# TYPE pos3ql_query_workspace_capacity gauge\n\
pos3ql_query_workspace_capacity {}\n\
# TYPE pos3ql_query_workspaces_active gauge\n\
pos3ql_query_workspaces_active {}\n\
# TYPE pos3ql_query_workspace_waiters gauge\n\
pos3ql_query_workspace_waiters {}\n\
# TYPE pos3ql_foreign_session_capacity gauge\n\
pos3ql_foreign_session_capacity {}\n\
# TYPE pos3ql_foreign_sessions_used gauge\n\
pos3ql_foreign_sessions_used {}\n\
# TYPE pos3ql_operational_connections gauge\n\
pos3ql_operational_connections {operations_active}\n\
# TYPE pos3ql_postgres_connections_accepted_total counter\n\
pos3ql_postgres_connections_accepted_total {}\n\
# TYPE pos3ql_postgres_connections_refused_total counter\n\
pos3ql_postgres_connections_refused_total {}\n\
# TYPE pos3ql_postgres_connections_closed_total counter\n\
pos3ql_postgres_connections_closed_total {}\n\
# TYPE pos3ql_operational_http_requests_total counter\n\
pos3ql_operational_http_requests_total {}\n\
# TYPE pos3ql_operational_http_errors_total counter\n\
pos3ql_operational_http_errors_total {}\n\
# TYPE pos3ql_object_store_credential_reload_successes_total counter\n\
pos3ql_object_store_credential_reload_successes_total {}\n\
# TYPE pos3ql_object_store_credential_reload_failures_total counter\n\
pos3ql_object_store_credential_reload_failures_total {}\n\
# TYPE pos3ql_wal_used_bytes gauge\n\
pos3ql_wal_used_bytes {}\n\
# TYPE pos3ql_core_memory_budget_bytes gauge\n\
pos3ql_core_memory_budget_bytes {}\n\
# TYPE pos3ql_tls_memory_budget_bytes gauge\n\
pos3ql_tls_memory_budget_bytes {}\n\
# TYPE pos3ql_wal_capacity_bytes gauge\n\
pos3ql_wal_capacity_bytes {}\n\
# TYPE pos3ql_row_heap_used_bytes gauge\n\
pos3ql_row_heap_used_bytes {}\n\
# TYPE pos3ql_row_heap_capacity_bytes gauge\n\
pos3ql_row_heap_capacity_bytes {}\n\
# TYPE pos3ql_checkpoint_pending gauge\n\
pos3ql_checkpoint_pending {pending}\n\
# TYPE pos3ql_block_cache_hits_total counter\n\
pos3ql_block_cache_hits_total {}\n\
# TYPE pos3ql_block_cache_misses_total counter\n\
pos3ql_block_cache_misses_total {}\n\
# TYPE pos3ql_disk_cache_hits_total counter\n\
pos3ql_disk_cache_hits_total {}\n\
# TYPE pos3ql_disk_cache_misses_total counter\n\
pos3ql_disk_cache_misses_total {}\n\
# TYPE pos3ql_block_object_gets_total counter\n\
pos3ql_block_object_gets_total {}\n\
# TYPE pos3ql_block_object_puts_total counter\n\
pos3ql_block_object_puts_total {}\n\
# TYPE pos3ql_block_object_read_bytes_total counter\n\
pos3ql_block_object_read_bytes_total {}\n\
# TYPE pos3ql_block_object_read_seconds_total counter\n\
pos3ql_block_object_read_seconds_total {:.6}\n\
# TYPE pos3ql_block_object_prefetch_saturated_total counter\n\
pos3ql_block_object_prefetch_saturated_total {}\n",
            snapshot.lsn,
            self.capacity_limits.postgres_connections,
            self.capacity_limits.query_workspace_slots,
            self.query_workspaces.used(),
            self.query_workspaces.waiting(),
            self.capacity_limits.foreign_sessions,
            snapshot.foreign_sessions_used,
            self.operations_metrics.postgres_accepted,
            self.operations_metrics.postgres_refused,
            self.operations_metrics.postgres_closed,
            self.operations_metrics.http_requests,
            self.operations_metrics.http_errors,
            self.operations_metrics.credential_reload_successes,
            self.operations_metrics.credential_reload_failures,
            snapshot.wal_used_bytes,
            self.memory_reserved_bytes,
            self.capacity_limits.tls_budget_bytes,
            snapshot.wal_capacity_bytes,
            snapshot.row_heap_used_bytes,
            snapshot.row_heap_capacity_bytes,
            io.ram_hits,
            io.ram_misses,
            io.disk_hits,
            io.disk_misses,
            io.object_gets,
            io.object_puts,
            io.object_read_bytes,
            io.object_read_micros as f64 / 1_000_000.0,
            io.object_prefetch_saturated,
        );
    }

    fn render_capacity(&self, out: &mut crate::util::StackStr<4096>) {
        use core::fmt::Write as _;
        let snapshot = self.engine.operational_snapshot();
        let limits = self.capacity_limits;
        let postgres_used = limits.postgres_connections - self.free.len();
        let operations_used = limits.operations_connections - self.operations_free.len();
        let _ = writeln!(
            out,
            "{{\"memory\":{{\"core_budget_bytes\":{},\"tls_budget_bytes\":{}}},\"postgres_connections\":{{\"used\":{postgres_used},\"limit\":{}}},\"query_workspace_slots\":{{\"used\":{},\"waiting\":{},\"limit\":{}}},\"foreign_sessions\":{{\"used\":{},\"limit\":{}}},\"operational_connections\":{{\"used\":{operations_used},\"limit\":{}}},\"wal_bytes\":{{\"used\":{},\"limit\":{}}},\"row_heap_bytes\":{{\"used\":{},\"limit\":{}}},\"cache_bytes\":{{\"memory_limit\":{},\"disk_limit\":{}}},\"temporary_spill_bytes\":{{\"limit\":{}}},\"catalog_limits\":{{\"tables\":{},\"indexes\":{},\"databases\":{},\"schemas\":{},\"roles\":{}}},\"prepared_transaction_limit\":{},\"replication_slot_limit\":{},\"subscription_limit\":{},\"object_store\":{},\"credential_rotation\":{}}}",
            self.memory_reserved_bytes,
            limits.tls_budget_bytes,
            limits.postgres_connections,
            self.query_workspaces.used(),
            self.query_workspaces.waiting(),
            limits.query_workspace_slots,
            snapshot.foreign_sessions_used,
            limits.foreign_sessions,
            limits.operations_connections,
            snapshot.wal_used_bytes,
            snapshot.wal_capacity_bytes,
            snapshot.row_heap_used_bytes,
            snapshot.row_heap_capacity_bytes,
            limits.block_cache_bytes,
            limits.disk_cache_bytes,
            limits.temporary_spill_bytes,
            limits.tables,
            limits.indexes,
            limits.databases,
            limits.schemas,
            limits.roles,
            limits.prepared_transactions,
            limits.replication_slots,
            limits.subscriptions,
            limits.object_store,
            limits.credential_rotation,
        );
    }

    fn queue_operations_error(&mut self, index: usize, status: u16, message: &str) {
        self.operations_metrics.http_requests =
            self.operations_metrics.http_requests.saturating_add(1);
        self.operations_metrics.http_errors = self.operations_metrics.http_errors.saturating_add(1);
        self.queue_operations_response(index, status, "text/plain; charset=utf-8", message);
    }

    fn queue_operations_response(
        &mut self,
        index: usize,
        status: u16,
        content_type: &str,
        body: &str,
    ) {
        use core::fmt::Write as _;
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            404 => "Not Found",
            405 => "Method Not Allowed",
            431 => "Request Header Fields Too Large",
            500 => "Internal Server Error",
            503 => "Service Unavailable",
            _ => "Error",
        };
        let slot = &mut self.operations_slots[index];
        slot.request.clear();
        slot.response.clear();
        let rendered = write!(
            slot.response,
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
            body.len(),
        )
        .is_ok();
        if !rendered {
            slot.response.clear();
            let _ = slot.response.append(
                b"HTTP/1.1 500 Internal Server Error\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            );
        }
        let fd = slot
            .stream
            .as_ref()
            .expect("open operational slot")
            .as_raw_fd();
        let token = Self::operations_token(index);
        if self.reactor.set_read_interest(fd, token, false).is_err()
            || self.reactor.set_write_interest(fd, token, true).is_err()
        {
            self.release_operations(index);
        }
    }

    fn write_operations(&mut self, index: usize) {
        let written = {
            let slot = &mut self.operations_slots[index];
            let stream = slot.stream.as_mut().expect("open operational slot");
            stream.write(slot.response.readable())
        };
        match written {
            Ok(0) => self.release_operations(index),
            Ok(bytes) => {
                self.operations_slots[index].response.consume(bytes);
                if self.operations_slots[index].response.is_empty() {
                    self.release_operations(index);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                log_io("write operational response", &error);
                self.release_operations(index);
            }
        }
    }

    fn release_operations(&mut self, index: usize) {
        let slot = &mut self.operations_slots[index];
        if let Some(stream) = slot.stream.take() {
            let _ = self.reactor.deregister(stream.as_raw_fd());
        }
        slot.request.clear();
        slot.response.clear();
        self.operations_free
            .push(index as u32)
            .expect("released operational slot cannot exceed capacity");
    }

    fn operations_slot(&self, token: u64) -> Option<usize> {
        let slot = token.checked_sub(OPERATIONS_TOKEN_BASE)? as usize;
        (slot < self.operations_slots.len()).then_some(slot)
    }

    fn operations_token(slot: usize) -> u64 {
        OPERATIONS_TOKEN_BASE + slot as u64
    }

    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    pub fn operations_local_addr(&self) -> Option<std::io::Result<std::net::SocketAddr>> {
        self.operations_listener
            .as_ref()
            .map(TcpListener::local_addr)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OperationsEndpoint {
    Live,
    Ready,
    Metrics,
    Capacity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HttpRequestError {
    Method,
    Target,
    Syntax,
}

fn parse_operations_request(
    request: &[u8],
) -> Result<Option<OperationsEndpoint>, HttpRequestError> {
    if !request.windows(4).any(|window| window == b"\r\n\r\n") {
        return Ok(None);
    }
    let line_end = request
        .windows(2)
        .position(|window| window == b"\r\n")
        .ok_or(HttpRequestError::Syntax)?;
    let mut fields = request[..line_end].split(|byte| *byte == b' ');
    let method = fields.next().ok_or(HttpRequestError::Syntax)?;
    let target = fields.next().ok_or(HttpRequestError::Syntax)?;
    let version = fields.next().ok_or(HttpRequestError::Syntax)?;
    if fields.next().is_some() || !matches!(version, b"HTTP/1.0" | b"HTTP/1.1") {
        return Err(HttpRequestError::Syntax);
    }
    if method != b"GET" {
        return Err(HttpRequestError::Method);
    }
    match target {
        b"/healthz" | b"/livez" => Ok(Some(OperationsEndpoint::Live)),
        b"/readyz" => Ok(Some(OperationsEndpoint::Ready)),
        b"/metrics" => Ok(Some(OperationsEndpoint::Metrics)),
        b"/capacity" => Ok(Some(OperationsEndpoint::Capacity)),
        _ => Err(HttpRequestError::Target),
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
    let message = core::str::from_utf8(msg).unwrap_or("invalid UTF-8 log message");
    crate::logging::info("server", message.trim_end());
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
        "{context}: kind={:?} os_error={:?}",
        e.kind(),
        e.raw_os_error()
    );
    crate::logging::error("io", message.as_str().trim_end());
}

fn log_subscription_error(
    name: Option<crate::storage::SqlName>,
    error: &crate::sql::eval::SqlError,
) {
    use core::fmt::Write as _;
    let mut message = crate::util::StackStr::<512>::new();
    let _ = writeln!(
        message,
        "subscription {} apply failed [{}]: {}",
        name.as_ref().map_or("<unknown>", |name| name.as_str()),
        error.sqlstate,
        error.message.as_str()
    );
    crate::logging::error("subscription_apply", message.as_str().trim_end());
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
                "subscription {} publisher failed [{}]: {}",
                name.as_ref().map_or("<unknown>", |name| name.as_str()),
                error.sqlstate,
                error.message.as_str()
            );
        }
        crate::pg::replication_client::ClientError::Io(error) => {
            let _ = writeln!(
                message,
                "subscription {} transport failed: kind={:?} os_error={:?}",
                name.as_ref().map_or("<unknown>", |name| name.as_str()),
                error.kind(),
                error.raw_os_error()
            );
        }
        _ => {
            let _ = writeln!(
                message,
                "subscription {} protocol failed: {:?}",
                name.as_ref().map_or("<unknown>", |name| name.as_str()),
                error
            );
        }
    }
    crate::logging::error("subscription_client", message.as_str().trim_end());
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::net::TcpStream;

    use super::{
        HttpRequestError, OPERATIONS_REQUEST_BYTES, OPERATIONS_RESPONSE_BYTES, OperationsEndpoint,
        OperationsSlot, QueryDispatch, QueryDispatchKind, QueryDispatchQueue, QueryWorkspaceLeases,
        Server, SubscriptionBinding, SubscriptionBootstrapStage, SubscriptionBootstrapWork,
        bind_listener, parse_operations_request,
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
            endpoint: crate::pg::replication_client::SubscriptionEndpoint::resolve(
                endpoint,
                crate::storage::SqlName::parse("apply").unwrap(),
            ),
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

    #[test]
    fn operational_http_parser_waits_for_headers_and_accepts_exact_get_targets() {
        assert_eq!(
            parse_operations_request(b"GET /readyz HTTP/1.1\r\n"),
            Ok(None)
        );
        for (target, endpoint) in [
            ("/healthz", OperationsEndpoint::Live),
            ("/livez", OperationsEndpoint::Live),
            ("/readyz", OperationsEndpoint::Ready),
            ("/metrics", OperationsEndpoint::Metrics),
            ("/capacity", OperationsEndpoint::Capacity),
        ] {
            let request = format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n");
            assert_eq!(
                parse_operations_request(request.as_bytes()),
                Ok(Some(endpoint))
            );
        }
    }

    #[test]
    fn operational_http_parser_rejects_unsupported_requests_loudly() {
        assert_eq!(
            parse_operations_request(b"POST /readyz HTTP/1.1\r\n\r\n"),
            Err(HttpRequestError::Method)
        );
        assert_eq!(
            parse_operations_request(b"GET /unknown HTTP/1.1\r\n\r\n"),
            Err(HttpRequestError::Target)
        );
        assert_eq!(
            parse_operations_request(b"GET /readyz HTTP/2\r\n\r\n"),
            Err(HttpRequestError::Syntax)
        );
    }

    #[test]
    fn operational_listener_memory_is_charged_exactly() {
        let disabled = crate::config::Config::default_dev();
        let mut enabled = disabled.clone();
        enabled.operations_listen_addr = "127.0.0.1:0".to_string();
        enabled.operations_max_connections = 7;
        let expected = crate::io::reactor::Reactor::budget_bytes(8)
            + 7 * (core::mem::size_of::<OperationsSlot>()
                + core::mem::size_of::<u32>()
                + OPERATIONS_REQUEST_BYTES
                + OPERATIONS_RESPONSE_BYTES);
        assert_eq!(
            Server::budget_bytes(&enabled) - Server::budget_bytes(&disabled),
            expected
        );
    }

    #[test]
    fn query_workspace_leases_are_bounded_exclusive_and_fifo() {
        let bytes = QueryWorkspaceLeases::budget_bytes(2, 4);
        let mut budget = crate::mem::budget::Budget::new(bytes);
        let mut leases = QueryWorkspaceLeases::new(&mut budget, 2, 4).unwrap();
        assert_eq!(budget.remaining(), 0);

        crate::mem::guard::forbid_alloc(|| {
            let first = leases.acquire(0).unwrap();
            let second = leases.acquire(1).unwrap();
            assert_ne!(first, second);
            assert_eq!(leases.acquire(0), Some(first));
            assert_eq!(leases.acquire(2), None);
            assert_eq!(leases.acquire(2), None);
            assert_eq!(leases.acquire(3), None);
            assert_eq!(leases.used(), 2);
            assert_eq!(leases.waiting(), 2);

            assert_eq!(leases.release_workspace(0, first), Some((2, first)));
            assert_eq!(leases.acquire(2), Some(first));
            assert_eq!(leases.waiting(), 1);
            assert_eq!(leases.cancel(2), Some((3, first)));
            assert_eq!(leases.acquire(3), Some(first));
            assert_eq!(leases.waiting(), 0);
            assert_eq!(leases.release_workspace(1, second), None);
            assert_eq!(leases.release_workspace(3, first), None);
            assert_eq!(leases.used(), 0);
        });
    }

    #[test]
    fn query_workspace_lease_memory_is_charged_exactly() {
        let mut smaller = crate::config::Config::default_dev();
        smaller.max_connections = 8;
        smaller.query_workspace_slots = 1;
        let mut larger = smaller.clone();
        larger.query_workspace_slots = 5;
        assert_eq!(
            Server::budget_bytes(&larger) - Server::budget_bytes(&smaller),
            4 * (core::mem::size_of::<Option<usize>>()
                + core::mem::size_of::<Option<QueryDispatch>>())
        );
    }

    #[test]
    fn query_dispatch_queue_is_bounded_allocation_free_and_fifo() {
        let bytes = QueryDispatchQueue::budget_bytes(3);
        let mut budget = crate::mem::budget::Budget::new(bytes);
        let mut queue = QueryDispatchQueue::new(&mut budget, 3).unwrap();
        assert_eq!(budget.remaining(), 0);

        let dispatch = |owner, generation, workspace, kind| QueryDispatch {
            owner,
            generation,
            workspace: crate::sql::QueryWorkspaceId::from_index(workspace),
            kind,
        };
        let first = dispatch(1, 11, 0, QueryDispatchKind::Readable);
        let second = dispatch(
            2,
            12,
            1,
            QueryDispatchKind::Retry {
                lock_generation: 7,
                retry_io_waiters: false,
            },
        );
        let third = dispatch(3, 13, 2, QueryDispatchKind::Readable);
        let wrapped = dispatch(4, 14, 0, QueryDispatchKind::Readable);
        crate::mem::guard::forbid_alloc(|| {
            queue.push(first);
            queue.push(second);
            queue.push(third);
            assert_eq!(queue.pop(), Some(first));
            queue.push(wrapped);
            assert_eq!(queue.pop(), Some(second));
            assert_eq!(queue.pop(), Some(third));
            assert_eq!(queue.pop(), Some(wrapped));
            assert_eq!(queue.pop(), None);
        });
    }

    #[test]
    fn cacheless_object_storage_uses_synchronous_block_reads() {
        let mut config = crate::config::Config::default_dev();
        config.object_store_on = true;
        config.object_store_get_slots = 7;
        config.block_cache_bytes = 0;
        config.disk_cache_bytes = 0;
        assert_eq!(Server::block_read_slots(&config), 0);

        config.block_cache_bytes = crate::store::MAX_PAYLOAD;
        assert_eq!(Server::block_read_slots(&config), 7);
        config.block_cache_bytes = 0;
        config.disk_cache_bytes = crate::store::BLOCK_SIZE;
        assert_eq!(Server::block_read_slots(&config), 7);
    }
}
