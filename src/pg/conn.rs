//! Per-connection protocol state machine. Owns fixed receive/send buffers
//! and the per-statement SQL arena; all of them are allocated once at
//! server startup and reused across connections.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::mem::FixedVec;
use crate::mem::arena::Arena;
use crate::mem::budget::{Budget, BudgetError};
use crate::mem::buffer::FixedBuf;
use crate::pg::auth::{AuthMode, ScramFlow, ScramServer, ScramStep};
use crate::sql::Engine;
use crate::sql::eval::SqlError;
use crate::sql::eval::sqlstate;
use crate::sql::guc::GucState;
use crate::sql::parser::Parser;
use crate::sql::prep::SqlPreparedPool;
use crate::sql::txn::TxnState;
use crate::sql::types::Datum;
use crate::sql_err;
use crate::stack_format;
use crate::storage::SqlName;
use crate::util::StackStr;

use super::REPORTED_SERVER_VERSION;
use super::respond::{MAX_RESULT_COLS, Responder, ResultFmt};
use super::wire::{self, MsgIn, WireFull};

/// Most parameters one Bind may carry.
pub const MAX_BIND_PARAMS: usize = 32;

/// Idle logical streams still need protocol traffic so a downstream can tell a
/// quiet publisher from a dead connection. PostgreSQL's `k` frame carries that
/// liveness signal; this fixed cadence is independent of client activity.
const REPLICATION_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);

fn reject_role_login(
    mode: AuthMode,
    role: crate::sql::RoleLogin,
    bootstrap_fallback: bool,
) -> bool {
    let missing_verifier =
        !matches!(mode, AuthMode::Trust) && role.password.is_none() && !bootstrap_fallback;
    !role.can_login || !role.valid || missing_verifier
}

fn reject_replication_login(mode: ReplicationMode, role: crate::sql::RoleLogin) -> bool {
    mode != ReplicationMode::None && !role.superuser && !role.replication
}

struct Prepared {
    active: bool,
    name: SqlName,
    text: FixedBuf,
    n_params: u16,
    /// Parameter type OIDs declared in Parse (0 = unspecified → text).
    param_oids: [i32; MAX_BIND_PARAMS],
}

struct Portal {
    active: bool,
    name: SqlName,
    statement: usize,
    params: FixedBuf,
    /// (offset, len) into `params`; `len == u32::MAX` marks NULL.
    spans: [(u32, u32); MAX_BIND_PARAMS],
    /// Per-parameter wire format: false = text, true = binary.
    binary: [bool; MAX_BIND_PARAMS],
    n_params: u16,
    /// Per-column result format requested by Bind.
    result_formats: ResultFmt,
    /// Buffered result messages for max_rows paging (Execute suspension).
    result: FixedBuf,
    executed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Waiting for a startup packet (or SSL/GSSENC probe).
    Startup,
    /// Cleartext password requested; waiting for PasswordMessage.
    AwaitPassword,
    /// SASL requested; waiting for SASLInitialResponse.
    AwaitSaslInit,
    /// SASL in flight; waiting for SASLResponse (client-final).
    AwaitSaslFinal,
    /// Normal message flow.
    Ready,
    /// Extended-protocol error recovery: discard until Sync.
    SkipToSync,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReplicationMode {
    None,
    Physical,
    Logical,
}

/// One active logical CopyBoth stream. All durable state remains in the slot;
/// this holds only the connection-owned cursor and negotiated stream settings.
struct ReplicationStream {
    slot: SqlName,
    /// pgoutput defaults to text; binary tuples require explicit negotiation.
    binary: bool,
    proto_version: u8,
    /// Durable slot confirmation, advanced only by a valid standby flush.
    cursor_lsn: u64,
    /// Connection-local send cursor. Reconnect starts again at `cursor_lsn`,
    /// while a live stream must not re-emit an already queued transaction.
    scan_lsn: u64,
    last_sent_lsn: u64,
    last_message_at: Instant,
    reply_requested: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StandbyStatusUpdate {
    flush_lsn: u64,
    reply_requested: bool,
}

/// Decodes the complete physical-replication status envelope shared by
/// logical CopyBoth streams. Acknowledgements describe one ordered receiver
/// frontier: bytes written are at least bytes flushed, which are at least
/// bytes applied.
fn standby_status_update(payload: &[u8]) -> Option<StandbyStatusUpdate> {
    if payload.len() != 34 || payload[0] != b'r' {
        return None;
    }
    let write_lsn = u64::from_be_bytes(payload[1..9].try_into().ok()?);
    let flush_lsn = u64::from_be_bytes(payload[9..17].try_into().ok()?);
    let apply_lsn = u64::from_be_bytes(payload[17..25].try_into().ok()?);
    let _client_time = i64::from_be_bytes(payload[25..33].try_into().ok()?);
    let reply_requested = match payload[33] {
        0 => false,
        1 => true,
        _ => return None,
    };
    (write_lsn >= flush_lsn && flush_lsn >= apply_lsn).then_some(StandbyStatusUpdate {
        flush_lsn,
        reply_requested,
    })
}

/// Server-wide authentication context, fixed at startup.
pub struct AuthContext {
    pub mode: AuthMode,
    pub password: String,
    pub scram: Option<ScramServer>,
}

/// What the server should do with the connection after an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum After {
    Continue,
    Close,
}

pub struct Conn {
    stream: Option<TcpStream>,
    pub recv: FixedBuf,
    pub send: FixedBuf,
    pub arena: Arena,
    pub txn: TxnState,
    pub sqlprep: SqlPreparedPool,
    pub cursors: crate::sql::cursor::CursorPool,
    pub guc: GucState,
    scram: ScramFlow,
    role_scram: Option<ScramServer>,
    auth_role_password: Option<crate::storage::RolePassword>,
    auth_login: Option<crate::sql::RoleLogin>,
    authenticated_role: Option<u16>,
    auth_password: StackStr<256>,
    auth_reject: bool,
    replication: ReplicationMode,
    replication_stream: Option<ReplicationStream>,
    /// Parsed pgoutput publication names. Its startup-sized capacity matches
    /// the configured bound on publications in storage.
    replication_publications: FixedVec<SqlName>,
    /// A COPY FROM STDIN in flight: the connection is in copy-in mode, and
    /// frontend messages are CopyData/CopyDone/CopyFail until it ends.
    copy: Option<CopyInProgress>,
    /// Staging for one COPY data row (rows split across CopyData messages).
    copy_buf: FixedBuf,
    prepared: Vec<Prepared>,
    portals: Vec<Portal>,
    phase: Phase,
    /// Negotiated protocol minor version (major is always 3).
    minor: u16,
    id: i32,
    /// A simple-query message parked on a row lock remains at the front of
    /// `recv`; completed statements in that message are not replayed.
    parked: bool,
    parked_generation: u64,
    /// I/O waits are resumed by the block-read completion event, whereas row
    /// locks wait for a changed lock generation or their timeout.
    parked_for_io: bool,
    /// Absolute deadline for the current lock acquisition. `None` means
    /// `lock_timeout = 0`; the reactor uses this to wake an otherwise idle
    /// server and retry the retained frontend message as a timeout error.
    parked_deadline: Option<Instant>,
    resume_statement: usize,
    /// The live TLS session, once the handshake starts. All socket bytes for
    /// `recv`/`send` tunnel through it.
    tls: Option<crate::pg::tls::ServerSession>,
    /// A TLS session staged by the SSLRequest handler, promoted to `tls` once
    /// the plaintext `S` acknowledgement has left the socket.
    pending_tls: Option<crate::pg::tls::ServerSession>,
}

impl Conn {
    pub fn new(config: &Config, budget: &mut Budget) -> Result<Self, BudgetError> {
        let empty = SqlName::parse("").expect("empty name fits");
        let mut prepared = Vec::with_capacity(config.max_prepared);
        for _ in 0..config.max_prepared {
            prepared.push(Prepared {
                active: false,
                name: empty,
                text: FixedBuf::new(budget, "prepared_text", config.prepared_bytes)?,
                n_params: 0,
                param_oids: [0; MAX_BIND_PARAMS],
            });
        }
        let mut portals = Vec::with_capacity(config.max_portals);
        for _ in 0..config.max_portals {
            portals.push(Portal {
                active: false,
                name: empty,
                statement: 0,
                params: FixedBuf::new(budget, "portal_params", config.portal_bytes)?,
                spans: [(0, 0); MAX_BIND_PARAMS],
                binary: [false; MAX_BIND_PARAMS],
                n_params: 0,
                result_formats: ResultFmt::ALL_TEXT,
                result: FixedBuf::new(budget, "portal_result", config.portal_result_bytes)?,
                executed: false,
            });
        }
        Ok(Self {
            stream: None,
            recv: FixedBuf::new(budget, "conn_recv", config.conn_recv_buffer_bytes)?,
            send: FixedBuf::new(budget, "conn_send", config.conn_send_buffer_bytes)?,
            arena: Arena::new(budget, "conn_sql_arena", config.sql_arena_bytes)?,
            txn: TxnState::new(budget, config.txn_rows)?,
            sqlprep: SqlPreparedPool::new(config, budget)?,
            cursors: crate::sql::cursor::CursorPool::new(config, budget)?,
            guc: GucState::new(),
            scram: ScramFlow::new(),
            role_scram: None,
            auth_role_password: None,
            auth_login: None,
            authenticated_role: None,
            auth_password: StackStr::new(),
            auth_reject: false,
            replication: ReplicationMode::None,
            replication_stream: None,
            replication_publications: FixedVec::new(
                budget,
                "replication_publications",
                config.max_tables,
            )?,
            copy: None,
            copy_buf: FixedBuf::new(budget, "copy_line", config.copy_line_bytes)?,
            prepared,
            portals,
            phase: Phase::Startup,
            minor: 0,
            id: 0,
            parked: false,
            parked_generation: 0,
            parked_for_io: false,
            parked_deadline: None,
            resume_statement: 0,
            tls: None,
            pending_tls: None,
        })
    }

    /// Binds this slot to a fresh socket, resetting all protocol state.
    pub fn open(&mut self, stream: TcpStream, id: i32) {
        self.stream = Some(stream);
        self.recv.clear();
        self.send.clear();
        self.arena.reset();
        self.txn.clear();
        self.sqlprep.clear();
        self.cursors.clear();
        for p in &mut self.prepared {
            p.active = false;
        }
        for p in &mut self.portals {
            p.active = false;
        }
        // A recycled slot must carry no session state from its previous client:
        // reset the GUCs (else a `SET` leaks across connections — SHOW/SET/
        // current_setting all read a stale value), the auth flow, and any COPY
        // left in flight by an abrupt disconnect.
        self.guc = GucState::new();
        self.scram = ScramFlow::new();
        self.role_scram = None;
        self.auth_role_password = None;
        self.auth_login = None;
        self.authenticated_role = None;
        self.auth_password = StackStr::new();
        self.auth_reject = false;
        self.replication = ReplicationMode::None;
        self.replication_stream = None;
        self.replication_publications.clear();
        self.copy = None;
        self.copy_buf.clear();
        self.phase = Phase::Startup;
        self.minor = 0;
        self.id = id;
        self.parked = false;
        self.parked_generation = 0;
        self.parked_for_io = false;
        self.parked_deadline = None;
        self.resume_statement = 0;
        self.tls = None;
        self.pending_tls = None;
    }

    pub fn close(&mut self) -> Option<TcpStream> {
        // Drop the TLS session (inside its scope, so the pool is credited)
        // before the socket goes.
        self.tls = None;
        self.pending_tls = None;
        self.stream.take()
    }

    pub fn is_open(&self) -> bool {
        self.stream.is_some()
    }

    pub fn stream(&self) -> &TcpStream {
        self.stream.as_ref().expect("connection is open")
    }

    pub fn wants_write(&self) -> bool {
        !self.send.is_empty() || self.tls.as_ref().is_some_and(|t| t.wants_write())
    }

    pub(crate) fn wants_read(&self) -> bool {
        !self.parked
    }

    /// The connection's id (the backend PID reported in BackendKeyData and in
    /// NotificationResponse).
    pub fn id(&self) -> i32 {
        self.id
    }

    pub(crate) fn authenticated_role(&self) -> Option<u16> {
        self.authenticated_role
    }

    /// Appends an asynchronous NotificationResponse ('A': int32 PID, channel,
    /// payload) to the send buffer. Returns false if it did not fit, leaving
    /// the buffer's queued messages intact; the caller then closes the
    /// connection rather than emit a truncated message.
    pub fn queue_notification(&mut self, pid: i32, channel: &str, payload: &str) -> bool {
        let mark = self.send.mark();
        let mut message = wire::MsgOut::begin(&mut self.send, wire::MSG_NOTIFICATION_RESPONSE);
        message.i32(pid);
        message.cstr(channel);
        message.cstr(payload);
        if message.finish().is_ok() {
            true
        } else {
            self.send.truncate_to(mark);
            false
        }
    }

    pub fn on_readable(
        &mut self,
        engine: &mut Engine,
        cancel_key: &[u8],
        auth: &AuthContext,
        tls_config: Option<&std::sync::Arc<rustls::ServerConfig>>,
    ) -> After {
        if self.stream.is_none() {
            return After::Close;
        }
        if self.parked {
            return match self.flush() {
                Ok(()) => After::Continue,
                Err(()) => After::Close,
            };
        }
        let space = self.recv.writable();
        if space.is_empty() {
            // Inbound message larger than the receive buffer: a protocol
            // limit, reported before closing.
            let mut responder = Responder::new(&mut self.send);
            let _ = responder.error(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "message exceeds the connection receive buffer",
            );
            let _ = self.flush();
            return After::Close;
        }
        // Decrypted plaintext when a TLS session is live; raw bytes otherwise.
        let read_result = if let Some(tls) = self.tls.as_mut() {
            tls.read(self.stream.as_mut().unwrap(), space)
        } else {
            self.stream.as_mut().unwrap().read(space)
        };
        match read_result {
            Ok(0) => return After::Close,
            Ok(n) => self.recv.advance(n),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return After::Close,
        }
        let after = self.process(engine, cancel_key, auth, tls_config);
        let flushed = self.flush();
        self.activate_pending_tls();
        match flushed {
            Ok(()) => after,
            Err(()) => After::Close,
        }
    }

    pub fn on_writable(&mut self) -> After {
        let flushed = self.flush();
        self.activate_pending_tls();
        // A TLS session mid-handshake may still owe the peer bytes after the
        // socket accepted what it could; keep the connection alive.
        match flushed {
            Ok(()) => After::Continue,
            Err(()) => After::Close,
        }
    }

    /// Promotes a TLS session staged by the SSLRequest handler once the
    /// plaintext `S` acknowledgement has fully left the send buffer, so the
    /// next socket bytes (the ClientHello) are read through the session.
    fn activate_pending_tls(&mut self) {
        if self.pending_tls.is_some() && self.send.is_empty() {
            self.tls = self.pending_tls.take();
        }
    }

    fn process(
        &mut self,
        engine: &mut Engine,
        cancel_key: &[u8],
        auth: &AuthContext,
        tls_config: Option<&std::sync::Arc<rustls::ServerConfig>>,
    ) -> After {
        loop {
            let after = match self.phase {
                Phase::Startup => self.process_startup(engine, cancel_key, auth, tls_config),
                Phase::AwaitPassword | Phase::AwaitSaslInit | Phase::AwaitSaslFinal => {
                    self.process_auth(engine, cancel_key, auth)
                }
                Phase::Ready | Phase::SkipToSync => self.process_message(engine),
            };
            match after {
                Step::NeedMoreData => return After::Continue,
                Step::Parked => return After::Continue,
                Step::Continue => {}
                Step::Close => return After::Close,
            }
        }
    }

    pub fn retry_parked(
        &mut self,
        engine: &mut Engine,
        generation: u64,
        retry_io_waiters: bool,
    ) -> After {
        if !self.parked
            || (self.parked_for_io && !retry_io_waiters)
            || (!self.parked_for_io
                && self.parked_generation == generation
                && !self.lock_timeout_expired())
        {
            return After::Continue;
        }
        self.parked = false;
        let after = match self.process_message(engine) {
            Step::Close => After::Close,
            Step::Continue | Step::NeedMoreData | Step::Parked => After::Continue,
        };
        match self.flush() {
            Ok(()) => after,
            Err(()) => After::Close,
        }
    }

    pub(crate) fn lock_wait_remaining(&self) -> Option<Duration> {
        if !self.parked {
            return None;
        }
        self.parked_deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    fn lock_timeout_expired(&self) -> bool {
        self.parked_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    fn park(&mut self, io_wait: bool, generation: u64) {
        self.parked = true;
        self.parked_generation = generation;
        self.parked_for_io = io_wait;
        if !io_wait && self.parked_deadline.is_none() {
            let timeout_ms = self.guc.lock_timeout_ms();
            if timeout_ms != 0 {
                self.parked_deadline =
                    Instant::now().checked_add(Duration::from_millis(timeout_ms));
            }
        }
    }

    fn finish_lock_wait(&mut self) {
        self.parked = false;
        self.parked_generation = 0;
        self.parked_for_io = false;
        self.parked_deadline = None;
    }

    fn process_startup(
        &mut self,
        engine: &mut Engine,
        cancel_key: &[u8],
        auth: &AuthContext,
        tls_config: Option<&std::sync::Arc<rustls::ServerConfig>>,
    ) -> Step {
        let data = self.recv.readable();
        if data.len() < 4 {
            return Step::NeedMoreData;
        }
        let len = i32::from_be_bytes(data[..4].try_into().unwrap());
        if !(8..=self.recv.capacity() as i32).contains(&len) {
            return Step::Close;
        }
        let len = len as usize;
        if data.len() < len {
            return Step::NeedMoreData;
        }
        let code = i32::from_be_bytes(data[4..8].try_into().unwrap());
        match code {
            wire::REQUEST_SSL => {
                self.recv.consume(len);
                match tls_config {
                    Some(config) if self.tls.is_none() && self.pending_tls.is_none() => {
                        // Acknowledge with 'S' in the clear; the session is
                        // staged and activated once that byte has been flushed,
                        // so the client's ClientHello is read through TLS.
                        match crate::pg::tls::ServerSession::new(config) {
                            Ok(session) => {
                                if !self.send.append(b"S") {
                                    return Step::Close;
                                }
                                self.pending_tls = Some(session);
                                Step::Continue
                            }
                            Err(_) => Step::Close,
                        }
                    }
                    _ => {
                        // TLS not configured (or already negotiated): decline,
                        // the client continues in the clear.
                        if !self.send.append(b"N") {
                            return Step::Close;
                        }
                        Step::Continue
                    }
                }
            }
            wire::REQUEST_GSSENC => {
                self.recv.consume(len);
                // GSSAPI encryption is not offered; 'N' declines it.
                if !self.send.append(b"N") {
                    return Step::Close;
                }
                Step::Continue
            }
            wire::REQUEST_CANCEL => {
                // Query cancellation needs cross-connection signalling;
                // the spec says just close if unsupported.
                Step::Close
            }
            version if version >> 16 == 3 => {
                let result = self.handle_startup_packet(engine, len, version, cancel_key, auth);
                self.recv.consume(len);
                result
            }
            _ => {
                let mut responder = Responder::new(&mut self.send);
                let _ = responder.error(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "unsupported protocol version (this server speaks 3.0-3.2)",
                );
                Step::Close
            }
        }
    }

    fn handle_startup_packet(
        &mut self,
        engine: &mut Engine,
        len: usize,
        version: i32,
        cancel_key: &[u8],
        auth: &AuthContext,
    ) -> Step {
        let requested_minor = (version & 0xffff) as u16;
        let payload = &self.recv.readable()[8..len];

        // Collect protocol extension options (there are none we support) so
        // NegotiateProtocolVersion can name them.
        let mut unknown_protocol_options = [""; 8];
        let mut n_unknown = 0;
        let mut user_seen = false;
        let mut guc_error: Option<crate::sql::eval::SqlError> = None;
        let mut msg = MsgIn::new(payload);
        loop {
            let Ok(key) = msg.cstr() else {
                return Step::Close;
            };
            if key.is_empty() {
                break;
            }
            let Ok(value) = msg.cstr() else {
                return Step::Close;
            };
            match key {
                "user" => {
                    user_seen = !value.is_empty();
                    self.guc.set_session_user(value);
                }
                "database" | "options" => {}
                "replication" => {
                    self.replication = match value {
                        "database" => ReplicationMode::Logical,
                        "true" | "on" | "yes" | "1" => ReplicationMode::Physical,
                        "false" | "off" | "no" | "0" => ReplicationMode::None,
                        _ => {
                            let mut responder = Responder::new(&mut self.send);
                            let _ = responder.error(
                                sqlstate::INVALID_PARAMETER_VALUE,
                                "invalid replication startup parameter",
                            );
                            return Step::Close;
                        }
                    };
                }
                _ if key.starts_with("_pq_.") => {
                    if n_unknown < unknown_protocol_options.len() {
                        // The name outlives the buffer read only within this
                        // call; NegotiateProtocolVersion is written before
                        // the packet is consumed.
                        unknown_protocol_options[n_unknown] = key;
                        n_unknown += 1;
                    }
                }
                // Recognized session GUCs (client_encoding, application_name,
                // DateStyle, TimeZone, ...) are applied to the per-session
                // store. A startup GUC we cannot honor rejects the connection,
                // as PostgreSQL does — never silently left at a wrong default.
                _ => {
                    if guc_error.is_none()
                        && let Err(e) = self.guc.set(key, value, false)
                    {
                        guc_error = Some(e);
                    }
                }
            }
        }
        if !user_seen {
            let mut responder = Responder::new(&mut self.send);
            let _ = responder.error(
                "28000",
                "no PostgreSQL user name specified in startup packet",
            );
            return Step::Close;
        }
        if let Some(e) = guc_error {
            let mut responder = Responder::new(&mut self.send);
            let _ = responder.error(e.sqlstate, e.message.as_str());
            return Step::Close;
        }

        self.minor = requested_minor.min(wire::NEWEST_MINOR as u16);
        self.auth_password = StackStr::new();
        self.auth_reject = false;
        self.role_scram = None;
        self.auth_role_password = None;
        self.auth_login = None;
        let session_user = self.guc.session_user();
        if let Some(role) = engine.role_login(session_user.as_str()) {
            self.auth_login = Some(role);
            let bootstrap_fallback =
                session_user.as_str() == "postgres" && !auth.password.is_empty();
            self.auth_reject = reject_role_login(auth.mode, role, bootstrap_fallback)
                || reject_replication_login(self.replication, role);
            if let Some(password) = role.password {
                self.auth_role_password = Some(password);
                self.role_scram = Some(ScramServer {
                    salt: password.salt,
                    stored_key: password.stored_key,
                    server_key: password.server_key,
                    iterations: password.iterations,
                });
            } else if bootstrap_fallback {
                self.auth_password = StackStr::from_str(&auth.password);
            }
        } else {
            // Unknown roles authenticate through the configured verifier only
            // to keep the exchange shape constant, but are always rejected.
            self.auth_password = StackStr::from_str(&auth.password);
            self.auth_reject = true;
        }

        // Version negotiation happens before any auth request.
        {
            let mut responder = Responder::new(&mut self.send);
            if (requested_minor > wire::NEWEST_MINOR as u16 || n_unknown > 0)
                && responder
                    .negotiate_protocol_version(
                        wire::NEWEST_MINOR,
                        &unknown_protocol_options[..n_unknown],
                    )
                    .is_err()
            {
                return Step::Close;
            }
        }

        match auth.mode {
            AuthMode::Trust if !self.auth_reject => self.finish_startup(engine, cancel_key),
            AuthMode::Trust => {
                let mut responder = Responder::new(&mut self.send);
                let _ = responder.error("28000", "role is not permitted to log in");
                Step::Close
            }
            AuthMode::Password => {
                let mut responder = Responder::new(&mut self.send);
                if responder.auth_cleartext_password().is_err() {
                    return Step::Close;
                }
                self.phase = Phase::AwaitPassword;
                Step::Continue
            }
            AuthMode::ScramSha256 => {
                let mut responder = Responder::new(&mut self.send);
                if responder.auth_sasl_mechanisms().is_err() {
                    return Step::Close;
                }
                self.scram = ScramFlow::new();
                self.phase = Phase::AwaitSaslInit;
                Step::Continue
            }
        }
    }

    /// AuthenticationOk, parameter statuses, key data, ReadyForQuery.
    fn finish_startup(&mut self, engine: &mut Engine, cancel_key: &[u8]) -> Step {
        if self.authenticated_role.is_none() {
            let Some(login) = self.auth_login else {
                return Step::Close;
            };
            if !engine.reserve_role_connection(login) {
                let mut responder = Responder::new(&mut self.send);
                let _ = responder.error(
                    sqlstate::TOO_MANY_CONNECTIONS,
                    "too many connections for role",
                );
                return Step::Close;
            }
            self.authenticated_role = Some(login.slot);
        }
        let minor = self.minor;
        let id = self.id;
        let mut responder = Responder::new(&mut self.send);
        let mut write_all = || -> Result<(), WireFull> {
            responder.auth_ok()?;
            for (k, v) in [
                ("server_version", REPORTED_SERVER_VERSION),
                ("server_encoding", "UTF8"),
                ("client_encoding", "UTF8"),
                ("DateStyle", "ISO, MDY"),
                ("integer_datetimes", "on"),
                ("standard_conforming_strings", "on"),
                ("TimeZone", "Etc/UTC"),
                ("in_hot_standby", "off"),
            ] {
                responder.parameter_status(k, v)?;
            }
            // 3.0 fixes the cancel key at 4 bytes; 3.2 allows up to 256.
            let key = if minor >= 2 {
                cancel_key
            } else {
                &cancel_key[..4]
            };
            responder.backend_key_data(id, key)?;
            responder.ready_for_query(b'I')?;
            Ok(())
        };
        if write_all().is_err() {
            return Step::Close;
        }
        self.phase = Phase::Ready;
        Step::Continue
    }

    /// Password / SASL messages during authentication.
    fn process_auth(&mut self, engine: &mut Engine, cancel_key: &[u8], auth: &AuthContext) -> Step {
        let data = self.recv.readable();
        if data.len() < 5 {
            return Step::NeedMoreData;
        }
        let msg_type = data[0];
        let len = i32::from_be_bytes(data[1..5].try_into().unwrap());
        if !(4..=(self.recv.capacity() - 1) as i32).contains(&len) {
            return Step::Close;
        }
        let total = 1 + len as usize;
        if data.len() < total {
            return Step::NeedMoreData;
        }
        if msg_type != wire::FMSG_PASSWORD {
            // Anything else during auth is a protocol violation.
            let mut responder = Responder::new(&mut self.send);
            let _ = responder.error(
                sqlstate::PROTOCOL_VIOLATION,
                "expected a password/SASL response during authentication",
            );
            return Step::Close;
        }
        let payload = &self.recv.readable()[5..total];

        let auth_failed = |send: &mut FixedBuf| -> Step {
            let mut responder = Responder::new(send);
            let _ = responder.error("28P01", "password authentication failed");
            Step::Close
        };

        let step = match self.phase {
            Phase::AwaitPassword => {
                let Ok(pass) = MsgIn::new(payload).cstr() else {
                    return Step::Close;
                };
                // Fixed-pattern comparison over both strings.
                let expected = self.auth_password.as_str();
                let ok = if let Some(verifier) = self.auth_role_password {
                    let candidate = ScramServer::derive(pass, verifier.salt, verifier.iterations);
                    candidate
                        .stored_key
                        .iter()
                        .zip(verifier.stored_key)
                        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
                        == 0
                } else {
                    pass.len() == expected.len()
                        && pass
                            .bytes()
                            .zip(expected.bytes())
                            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                            == 0
                } && !self.auth_reject;
                if ok {
                    self.finish_startup(engine, cancel_key)
                } else {
                    auth_failed(&mut self.send)
                }
            }
            Phase::AwaitSaslInit => {
                let Some(server) = self.role_scram.as_ref().or(auth.scram.as_ref()) else {
                    return Step::Close;
                };
                let mut m = MsgIn::new(payload);
                let (Ok(mechanism), Ok(resp_len)) = (m.cstr(), m.i32()) else {
                    return Step::Close;
                };
                if mechanism != "SCRAM-SHA-256" || resp_len < 0 {
                    return auth_failed(&mut self.send);
                }
                let Ok(body) = m.take(resp_len as usize) else {
                    return Step::Close;
                };
                let Ok(client_first) = core::str::from_utf8(body) else {
                    return Step::Close;
                };
                let mut nonce = [0u8; 18];
                let rc = unsafe { libc::getentropy(nonce.as_mut_ptr().cast(), nonce.len()) };
                if rc != 0 {
                    return Step::Close;
                }
                match self.scram.first(server, client_first, &nonce) {
                    Ok(ScramStep::Continue(payload)) => {
                        let mut responder = Responder::new(&mut self.send);
                        if responder.auth_sasl_continue(payload.as_str()).is_err() {
                            return Step::Close;
                        }
                        self.phase = Phase::AwaitSaslFinal;
                        Step::Continue
                    }
                    _ => auth_failed(&mut self.send),
                }
            }
            Phase::AwaitSaslFinal => {
                let Some(server) = self.role_scram.as_ref().or(auth.scram.as_ref()) else {
                    return Step::Close;
                };
                let Ok(client_final) = core::str::from_utf8(payload) else {
                    return Step::Close;
                };
                match self.scram.finish(server, client_final) {
                    Ok(ScramStep::Final(_)) if self.auth_reject => auth_failed(&mut self.send),
                    Ok(ScramStep::Final(sig)) => {
                        {
                            let mut responder = Responder::new(&mut self.send);
                            if responder.auth_sasl_final(sig.as_str()).is_err() {
                                return Step::Close;
                            }
                        }
                        self.finish_startup(engine, cancel_key)
                    }
                    _ => auth_failed(&mut self.send),
                }
            }
            _ => unreachable!("process_auth only runs in auth phases"),
        };
        self.recv.consume(total);
        step
    }

    fn process_message(&mut self, engine: &mut Engine) -> Step {
        let data = self.recv.readable();
        if data.len() < 5 {
            return Step::NeedMoreData;
        }
        let msg_type = data[0];
        let len = i32::from_be_bytes(data[1..5].try_into().unwrap());
        if !(4..=(self.recv.capacity() - 1) as i32).contains(&len) {
            return Step::Close;
        }
        let total = 1 + len as usize;
        if data.len() < total {
            return Step::NeedMoreData;
        }

        if self.replication != ReplicationMode::None
            && !(self.replication_stream.is_some()
                && matches!(
                    msg_type,
                    wire::FMSG_COPY_DATA | wire::FMSG_TERMINATE | wire::FMSG_FLUSH
                ))
            && !matches!(
                msg_type,
                wire::FMSG_QUERY | wire::FMSG_TERMINATE | wire::FMSG_FLUSH
            )
        {
            let mut responder = Responder::new(&mut self.send);
            let _ = responder.error(
                sqlstate::PROTOCOL_VIOLATION,
                "replication connections accept only the simple query protocol",
            );
            return Step::Close;
        }

        if self.replication_stream.is_some() {
            let step = self.process_replication_stream_message(engine, msg_type, total);
            if !matches!(step, Step::Close) {
                self.recv.consume(total);
            }
            return step;
        }

        if self.copy.is_some() {
            let step = self.process_copy_message(engine, msg_type, total);
            if !matches!(step, Step::Close) {
                self.recv.consume(total);
            }
            return step;
        }

        if self.phase == Phase::SkipToSync {
            let is_sync = msg_type == wire::FMSG_SYNC;
            self.recv.consume(total);
            if is_sync {
                self.phase = Phase::Ready;
                let status = self.txn.status_byte();
                let mut responder = Responder::new(&mut self.send);
                if responder.ready_for_query(status).is_err() {
                    return Step::Close;
                }
            }
            return Step::Continue;
        }

        let step = match msg_type {
            wire::FMSG_QUERY => self.handle_query(engine, total),
            wire::FMSG_TERMINATE => Step::Close,
            wire::FMSG_SYNC => {
                let status = self.txn.status_byte();
                let mut responder = Responder::new(&mut self.send);
                match responder.ready_for_query(status) {
                    Ok(()) => Step::Continue,
                    Err(WireFull) => Step::Close,
                }
            }
            wire::FMSG_FLUSH => Step::Continue,
            wire::FMSG_PARSE => self.handle_parse(total),
            wire::FMSG_BIND => self.handle_bind(total),
            wire::FMSG_DESCRIBE => self.handle_describe(engine, total),
            wire::FMSG_EXECUTE => self.handle_execute(engine, total),
            wire::FMSG_CLOSE => self.handle_close(total),
            _ => {
                let mut responder = Responder::new(&mut self.send);
                let _ = responder.error(
                    sqlstate::PROTOCOL_VIOLATION,
                    "unknown frontend message type",
                );
                Step::Close
            }
        };
        if !matches!(step, Step::Close | Step::Parked) && msg_type != wire::FMSG_QUERY {
            self.recv.consume(total);
        }
        step
    }

    /// Frontend traffic while a COPY FROM STDIN is in flight. CopyData
    /// chunks accumulate into whole lines; the first error stops storing
    /// but keeps draining, as PostgreSQL does; CopyDone settles the
    /// transaction and answers; CopyFail aborts on the client's behalf.
    fn process_copy_message(&mut self, engine: &mut Engine, msg_type: u8, total: usize) -> Step {
        match msg_type {
            wire::FMSG_COPY_DATA => {
                self.copy_data_chunk(engine, total);
                Step::Continue
            }
            wire::FMSG_COPY_DONE => self.copy_finish(engine),
            wire::FMSG_COPY_FAIL => {
                let message = MsgIn::new(&self.recv.readable()[5..total])
                    .cstr()
                    .unwrap_or("client sent an invalid CopyFail message");
                let detail = crate::stack_format!(256, "COPY from stdin failed: {message}");
                let extended = self.copy.as_ref().expect("in copy-in mode").extended;
                engine.copy_abort(&mut self.txn, &self.guc);
                self.copy = None;
                self.copy_buf.clear();
                let mut responder = Responder::new(&mut self.send);
                let sent = responder.error(sqlstate::QUERY_CANCELED, detail.as_str());
                if extended {
                    self.phase = Phase::SkipToSync;
                }
                let sent = if extended {
                    sent
                } else {
                    sent.and_then(|()| responder.ready_for_query(self.txn.status_byte()))
                };
                if sent.is_err() {
                    Step::Close
                } else {
                    Step::Continue
                }
            }
            wire::FMSG_TERMINATE => Step::Close,
            // Flush and Sync during copy-in are ignored, as PostgreSQL does.
            wire::FMSG_SYNC | wire::FMSG_FLUSH => Step::Continue,
            _ => {
                let extended = self.copy.as_ref().expect("in copy-in mode").extended;
                engine.copy_abort(&mut self.txn, &self.guc);
                self.copy = None;
                self.copy_buf.clear();
                let mut responder = Responder::new(&mut self.send);
                let sent = responder.error(
                    sqlstate::PROTOCOL_VIOLATION,
                    "unexpected message type during COPY from stdin",
                );
                if extended {
                    self.phase = Phase::SkipToSync;
                }
                let sent = if extended {
                    sent
                } else {
                    sent.and_then(|()| responder.ready_for_query(self.txn.status_byte()))
                };
                if sent.is_err() {
                    Step::Close
                } else {
                    Step::Continue
                }
            }
        }
    }

    fn process_replication_stream_message(
        &mut self,
        engine: &mut Engine,
        msg_type: u8,
        total: usize,
    ) -> Step {
        match msg_type {
            wire::FMSG_COPY_DATA => {
                let payload = &self.recv.readable()[5..total];
                let Some(status) = standby_status_update(payload) else {
                    return Step::Close;
                };
                let stream = self
                    .replication_stream
                    .as_mut()
                    .expect("active replication stream");
                if status.flush_lsn > stream.last_sent_lsn {
                    return Step::Close;
                }
                if status.flush_lsn > stream.cursor_lsn {
                    if engine
                        .advance_replication_slot(stream.slot.as_str(), status.flush_lsn)
                        .is_err()
                    {
                        return Step::Close;
                    }
                    stream.cursor_lsn = status.flush_lsn;
                }
                stream.reply_requested = status.reply_requested;
                Step::Continue
            }
            wire::FMSG_FLUSH => Step::Continue,
            wire::FMSG_TERMINATE => Step::Close,
            _ => Step::Close,
        }
    }

    /// Emits at most one complete transaction each reactor turn. A full send
    /// buffer leaves the durable cursor unchanged, so reconnect/retry cannot
    /// skip a transaction.
    pub(crate) fn pump_replication(&mut self, engine: &mut Engine) -> After {
        if !self.send.is_empty() {
            return After::Continue;
        }
        let Some(stream) = self.replication_stream.as_mut() else {
            return After::Continue;
        };
        let mark = self.send.mark();
        let mut responder = Responder::new(&mut self.send);
        let now = Instant::now();
        match engine.emit_replication_transaction(
            stream.scan_lsn,
            self.replication_publications.as_slice(),
            stream.binary,
            stream.proto_version,
            &mut self.copy_buf,
            &mut responder,
        ) {
            Ok(Some((lsn, emitted))) => {
                stream.scan_lsn = lsn;
                if emitted {
                    stream.last_sent_lsn = lsn;
                    stream.last_message_at = now;
                }
                After::Continue
            }
            Ok(None) => {
                if !stream.reply_requested
                    && now.duration_since(stream.last_message_at) < REPLICATION_KEEPALIVE_INTERVAL
                {
                    return After::Continue;
                }
                let (_, wal_end) = engine.replication_identity();
                if responder
                    .copy_data(&|message| {
                        message.u8(b'k');
                        message.i64(wal_end as i64);
                        message.i64(crate::sql::datetime::now_micros());
                        message.u8(u8::from(stream.reply_requested));
                    })
                    .is_err()
                {
                    self.send.truncate_to(mark);
                    return After::Close;
                }
                stream.last_sent_lsn = stream.last_sent_lsn.max(wal_end);
                stream.last_message_at = now;
                stream.reply_requested = false;
                After::Continue
            }
            Err(_) => {
                self.send.truncate_to(mark);
                After::Close
            }
        }
    }

    pub(crate) fn stop_replication(&mut self, engine: &mut Engine) {
        if let Some(stream) = self.replication_stream.take() {
            engine.deactivate_replication_slot(stream.slot.as_str());
        }
        self.replication_publications.clear();
    }

    /// The next time an idle CopyBoth stream must be serviced. This gives the
    /// reactor an explicit wakeup edge instead of making replication liveness
    /// depend on unrelated socket traffic.
    pub(crate) fn replication_keepalive_remaining(&self) -> Option<Duration> {
        let stream = self.replication_stream.as_ref()?;
        if !self.send.is_empty() || stream.reply_requested {
            return Some(Duration::ZERO);
        }
        Some(
            REPLICATION_KEEPALIVE_INTERVAL
                .saturating_sub(Instant::now().duration_since(stream.last_message_at)),
        )
    }

    fn copy_data_chunk(&mut self, engine: &mut Engine, total: usize) {
        // Stage the chunk; a row larger than the staging buffer is the
        // COPY's first error (drained like any other).
        let overflowed = {
            let payload = &self.recv.readable()[5..total];
            !self.copy_buf.append(payload)
        };
        if overflowed {
            let copy = self.copy.as_mut().expect("in copy-in mode");
            if copy.failed.is_none() {
                copy.failed = Some(crate::sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "COPY row exceeds copy_line_bytes ({})",
                    self.copy_buf.capacity()
                ));
            }
        }
        // Binary framing is length-based, not line-based.
        if self
            .copy
            .as_ref()
            .expect("in copy-in mode")
            .setup
            .fmt
            .binary
        {
            self.copy_binary_chunk(engine);
            return;
        }
        let copy = self.copy.as_mut().expect("in copy-in mode");
        let csv = copy.setup.fmt.csv;
        let (quote, escape) = (copy.setup.fmt.quote, copy.setup.fmt.escape);
        loop {
            // A CSV row can carry a newline inside a quoted field, so its end is
            // found quote-aware, not at the first newline.
            let readable = self.copy_buf.readable();
            let line_end = if csv {
                crate::sql::copy::csv_row_len(readable, quote, escape)
            } else {
                readable.iter().position(|&b| b == b'\n')
            };
            let Some(line_end) = line_end else {
                // No complete row; if the buffer is full without one, the row
                // cannot ever complete.
                if self.copy_buf.len() == self.copy_buf.capacity() && copy.failed.is_none() {
                    copy.failed = Some(crate::sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "COPY row exceeds copy_line_bytes ({})",
                        self.copy_buf.capacity()
                    ));
                }
                return;
            };
            self.arena.reset();
            {
                let line = &self.copy_buf.readable()[..line_end];
                if copy.header_pending {
                    // The first line is the header of column names: skip it.
                    copy.header_pending = false;
                } else if copy.end_seen || copy.failed.is_some() {
                    // Draining: data after the end marker or an error is read
                    // and dropped, never stored.
                } else if crate::sql::copy::is_end_marker(line) {
                    copy.end_seen = true;
                } else {
                    match engine.copy_row_line(
                        &copy.setup,
                        &mut self.txn,
                        self.guc.seq_session(),
                        &self.arena,
                        line,
                    ) {
                        Ok(()) => copy.count += 1,
                        Err(e) => copy.failed = Some(e),
                    }
                }
            }
            self.copy_buf.consume(line_end + 1);
        }
    }

    /// COPY FROM BINARY: consume the file header once, then decode each
    /// length-framed row until the -1 trailer. A row spanning several CopyData
    /// chunks is assembled in `copy_buf` before it is decoded.
    fn copy_binary_chunk(&mut self, engine: &mut Engine) {
        use crate::sql::copy::{BinaryFrame, BinaryHeader, binary_frame, binary_header};
        let copy = self.copy.as_mut().expect("in copy-in mode");
        if copy.binary_header_pending {
            match binary_header(self.copy_buf.readable()) {
                BinaryHeader::Incomplete => {
                    if self.copy_buf.len() == self.copy_buf.capacity() && copy.failed.is_none() {
                        copy.failed = Some(crate::sql_err!(
                            sqlstate::BAD_COPY_FILE_FORMAT,
                            "COPY binary header exceeds the buffer"
                        ));
                    }
                    return;
                }
                BinaryHeader::Bad => {
                    if copy.failed.is_none() {
                        copy.failed = Some(crate::sql_err!(
                            sqlstate::BAD_COPY_FILE_FORMAT,
                            "COPY file signature not recognized"
                        ));
                    }
                    copy.binary_header_pending = false;
                    self.copy_buf.clear();
                    return;
                }
                BinaryHeader::Done(len) => {
                    copy.binary_header_pending = false;
                    self.copy_buf.consume(len);
                }
            }
        }
        loop {
            match binary_frame(self.copy_buf.readable()) {
                BinaryFrame::Incomplete => {
                    if self.copy_buf.len() == self.copy_buf.capacity() && copy.failed.is_none() {
                        copy.failed = Some(crate::sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "COPY binary row exceeds copy_line_bytes ({})",
                            self.copy_buf.capacity()
                        ));
                    }
                    return;
                }
                BinaryFrame::Bad => {
                    if copy.failed.is_none() {
                        copy.failed = Some(crate::sql_err!(
                            sqlstate::BAD_COPY_FILE_FORMAT,
                            "malformed COPY binary field length"
                        ));
                    }
                    self.copy_buf.clear();
                    return;
                }
                BinaryFrame::Trailer => {
                    copy.end_seen = true;
                    self.copy_buf.consume(2);
                }
                BinaryFrame::Row(len) => {
                    self.arena.reset();
                    if !copy.end_seen && copy.failed.is_none() {
                        let row = &self.copy_buf.readable()[..len];
                        match engine.copy_row_binary(
                            &copy.setup,
                            &mut self.txn,
                            self.guc.seq_session(),
                            &self.arena,
                            row,
                        ) {
                            Ok(()) => copy.count += 1,
                            Err(e) => copy.failed = Some(e),
                        }
                    }
                    self.copy_buf.consume(len);
                }
            }
        }
    }

    fn copy_finish(&mut self, engine: &mut Engine) -> Step {
        // Binary rows are fully consumed as they complete in copy_binary_chunk;
        // any leftover at CopyDone is a truncated stream, but the count already
        // reflects the rows that landed, so nothing more is decoded here.
        let binary = self
            .copy
            .as_ref()
            .expect("in copy-in mode")
            .setup
            .fmt
            .binary;
        // A final line without a trailing newline is still a line (text/CSV).
        if !binary && !self.copy_buf.is_empty() {
            let copy = self.copy.as_mut().expect("in copy-in mode");
            self.arena.reset();
            let line = self.copy_buf.readable();
            if copy.header_pending {
                // A header line with no trailing newline: skip it.
                copy.header_pending = false;
            } else if !copy.end_seen
                && copy.failed.is_none()
                && !crate::sql::copy::is_end_marker(line)
            {
                match engine.copy_row_line(
                    &copy.setup,
                    &mut self.txn,
                    self.guc.seq_session(),
                    &self.arena,
                    line,
                ) {
                    Ok(()) => copy.count += 1,
                    Err(e) => copy.failed = Some(e),
                }
            }
            self.copy_buf.clear();
        }
        let copy = self.copy.take().expect("in copy-in mode");
        let extended = copy.extended;
        let outcome = match copy.failed {
            Some(e) => {
                engine.copy_abort(&mut self.txn, &self.guc);
                Err(e)
            }
            None => engine
                .copy_finish(&mut self.txn, &self.guc)
                .map(|()| copy.count),
        };
        let failed = outcome.is_err();
        let mut responder = Responder::new(&mut self.send);
        let sent = match outcome {
            Ok(count) => {
                responder.command_complete(crate::stack_format!(32, "COPY {count}").as_str())
            }
            Err(e) => responder.error(e.sqlstate, e.message.as_str()),
        };
        if extended && failed {
            self.phase = Phase::SkipToSync;
        }
        let sent = if extended {
            sent
        } else {
            sent.and_then(|()| responder.ready_for_query(self.txn.status_byte()))
        };
        if sent.is_err() {
            Step::Close
        } else {
            Step::Continue
        }
    }

    fn handle_parse(&mut self, total: usize) -> Step {
        let payload = &self.recv.readable()[5..total];
        let mut msg = MsgIn::new(payload);
        let parse = || -> Result<(&str, &str, [i32; MAX_BIND_PARAMS]), wire::Malformed> {
            let mut m = MsgIn::new(payload);
            let name = m.cstr()?;
            let query = m.cstr()?;
            let n_types = m.i16()?.max(0) as usize;
            let mut oids = [0i32; MAX_BIND_PARAMS];
            for i in 0..n_types {
                let oid = m.i32()?;
                if let Some(slot) = oids.get_mut(i) {
                    *slot = oid;
                }
            }
            Ok((name, query, oids))
        };
        let _ = &mut msg;
        let Ok((name, query, param_oids)) = parse() else {
            return ext_err(
                &mut self.send,
                &mut self.phase,
                sqlstate::PROTOCOL_VIOLATION,
                "malformed Parse message",
            );
        };

        // Validate now so Parse errors surface at Parse, like PostgreSQL.
        self.arena.reset();
        let n_params = {
            let mut parser = match Parser::new(query, &self.arena) {
                Ok(p) => p,
                Err(e) => {
                    return ext_err(
                        &mut self.send,
                        &mut self.phase,
                        sqlstate::SYNTAX_ERROR,
                        e.message.as_str(),
                    );
                }
            };
            match parser.next_stmt() {
                Ok(_first) => {}
                Err(e) => {
                    return ext_err(
                        &mut self.send,
                        &mut self.phase,
                        sqlstate::SYNTAX_ERROR,
                        e.message.as_str(),
                    );
                }
            }
            match parser.next_stmt() {
                Ok(None) => {}
                _ => {
                    return ext_err(
                        &mut self.send,
                        &mut self.phase,
                        sqlstate::SYNTAX_ERROR,
                        "cannot insert multiple commands into a prepared statement",
                    );
                }
            }
            parser.max_param()
        };
        if n_params as usize > MAX_BIND_PARAMS {
            return ext_err(
                &mut self.send,
                &mut self.phase,
                crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many parameters (the limit is 32)",
            );
        }

        // Named statements may not be redefined; the unnamed one always is.
        let slot = if name.is_empty() {
            self.prepared
                .iter()
                .position(|p| p.active && p.name.as_str().is_empty())
                .or_else(|| self.prepared.iter().position(|p| !p.active))
        } else if self
            .prepared
            .iter()
            .any(|p| p.active && p.name.as_str() == name)
        {
            return ext_err(
                &mut self.send,
                &mut self.phase,
                crate::sql::eval::sqlstate::DUPLICATE_PREPARED_STATEMENT,
                "prepared statement already exists",
            );
        } else {
            self.prepared.iter().position(|p| !p.active)
        };
        let Some(slot) = slot else {
            return ext_err(
                &mut self.send,
                &mut self.phase,
                "54000",
                "too many prepared statements",
            );
        };
        let Ok(sql_name) = SqlName::parse(name) else {
            return ext_err(
                &mut self.send,
                &mut self.phase,
                "42622",
                "statement name too long",
            );
        };
        let entry = &mut self.prepared[slot];
        entry.text.clear();
        if !entry.text.append(query.as_bytes()) {
            entry.active = false;
            return ext_err(
                &mut self.send,
                &mut self.phase,
                "54000",
                "statement text exceeds prepared_bytes",
            );
        }
        entry.active = true;
        entry.name = sql_name;
        entry.n_params = n_params as u16;
        entry.param_oids = param_oids;

        let mut responder = Responder::new(&mut self.send);
        match responder.parse_complete() {
            Ok(()) => Step::Continue,
            Err(WireFull) => Step::Close,
        }
    }

    fn handle_bind(&mut self, total: usize) -> Step {
        enum BindProblem {
            Malformed,
            TooManyResultCols,
            TooManyParams,
        }
        type BindParts<'a> = (
            &'a str,
            &'a str,
            usize,
            [(u32, u32); MAX_BIND_PARAMS],
            [bool; MAX_BIND_PARAMS],
            &'a [u8],
            ResultFmt,
        );
        let payload = &self.recv.readable()[5..total];
        let parse = || -> Result<BindParts<'_>, BindProblem> {
            let mut m = MsgIn::new(payload);
            let portal = m.cstr().map_err(|_| BindProblem::Malformed)?;
            let statement = m.cstr().map_err(|_| BindProblem::Malformed)?;
            let n_fmt = m.i16().map_err(|_| BindProblem::Malformed)?.max(0) as usize;
            let mut formats = [false; MAX_BIND_PARAMS];
            let mut uniform: Option<bool> = None;
            for i in 0..n_fmt {
                let binary = m.i16().map_err(|_| BindProblem::Malformed)? == 1;
                if n_fmt == 1 {
                    uniform = Some(binary);
                } else if let Some(slot) = formats.get_mut(i) {
                    *slot = binary;
                }
            }
            let n_params = m.i16().map_err(|_| BindProblem::Malformed)?.max(0) as usize;
            if n_params > MAX_BIND_PARAMS {
                return Err(BindProblem::TooManyParams);
            }
            if let Some(all) = uniform {
                formats = [all; MAX_BIND_PARAMS];
            }
            let values_start = payload.len() - m.remaining();
            let mut spans = [(0u32, 0u32); MAX_BIND_PARAMS];
            for span in spans.iter_mut().take(n_params) {
                let len = m.i32().map_err(|_| BindProblem::Malformed)?;
                if len < 0 {
                    *span = (0, u32::MAX);
                } else {
                    let at = payload.len() - m.remaining();
                    m.take(len as usize).map_err(|_| BindProblem::Malformed)?;
                    *span = ((at - values_start) as u32, len as u32);
                }
            }
            let values = &payload[values_start..payload.len() - m.remaining()];
            let n_rfmt = m.i16().map_err(|_| BindProblem::Malformed)?.max(0) as usize;
            let mut rcodes = [false; MAX_RESULT_COLS];
            for i in 0..n_rfmt {
                let binary = m.i16().map_err(|_| BindProblem::Malformed)? == 1;
                if let Some(slot) = rcodes.get_mut(i) {
                    *slot = binary;
                } else if binary {
                    // A binary format beyond the tracked column count cannot be
                    // honored; reject rather than silently emitting text.
                    return Err(BindProblem::TooManyResultCols);
                }
            }
            let result_formats = ResultFmt::new(rcodes, n_rfmt.min(MAX_RESULT_COLS) as u16);
            Ok((
                portal,
                statement,
                n_params,
                spans,
                formats,
                values,
                result_formats,
            ))
        };
        let (portal_name, stmt_name, n_params, spans, formats, values, result_formats) =
            match parse() {
                Ok(x) => x,
                Err(BindProblem::Malformed) => {
                    return ext_err(
                        &mut self.send,
                        &mut self.phase,
                        sqlstate::PROTOCOL_VIOLATION,
                        "malformed Bind message",
                    );
                }
                Err(BindProblem::TooManyResultCols) => {
                    return ext_err(
                        &mut self.send,
                        &mut self.phase,
                        crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "too many result columns requested in binary format",
                    );
                }
                Err(BindProblem::TooManyParams) => {
                    return ext_err(
                        &mut self.send,
                        &mut self.phase,
                        crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "too many parameters (the limit is 32)",
                    );
                }
            };

        let Some(stmt_slot) = self
            .prepared
            .iter()
            .position(|p| p.active && p.name.as_str() == stmt_name)
        else {
            return ext_err(
                &mut self.send,
                &mut self.phase,
                "26000",
                "prepared statement does not exist",
            );
        };
        if n_params != self.prepared[stmt_slot].n_params as usize {
            return ext_err(
                &mut self.send,
                &mut self.phase,
                sqlstate::PROTOCOL_VIOLATION,
                "bind parameter count differs from the statement",
            );
        }
        // Text-format parameters must be valid UTF-8, checked at bind time.
        for (i, &(offset, len)) in spans.iter().take(n_params).enumerate() {
            if !formats[i]
                && len != u32::MAX
                && core::str::from_utf8(&values[offset as usize..(offset + len) as usize]).is_err()
            {
                return ext_err(
                    &mut self.send,
                    &mut self.phase,
                    "22021",
                    "invalid UTF-8 in parameter value",
                );
            }
        }

        let slot = self
            .portals
            .iter()
            .position(|p| p.active && p.name.as_str() == portal_name)
            .or_else(|| self.portals.iter().position(|p| !p.active));
        let Some(slot) = slot else {
            return ext_err(&mut self.send, &mut self.phase, "54000", "too many portals");
        };
        let Ok(sql_name) = SqlName::parse(portal_name) else {
            return ext_err(
                &mut self.send,
                &mut self.phase,
                "42622",
                "portal name too long",
            );
        };
        // Copy the raw parameter area; spans index into it.
        let portal = &mut self.portals[slot];
        portal.params.clear();
        if !portal.params.append(values) {
            return ext_err(
                &mut self.send,
                &mut self.phase,
                "54000",
                "parameters exceed portal_bytes",
            );
        }
        portal.active = true;
        portal.name = sql_name;
        portal.statement = stmt_slot;
        portal.spans = spans;
        portal.binary = formats;
        portal.n_params = n_params as u16;
        portal.result_formats = result_formats;
        portal.result.clear();
        portal.executed = false;

        let mut responder = Responder::new(&mut self.send);
        match responder.bind_complete() {
            Ok(()) => Step::Continue,
            Err(WireFull) => Step::Close,
        }
    }

    fn handle_describe(&mut self, engine: &mut Engine, total: usize) -> Step {
        let payload = &self.recv.readable()[5..total];
        let mut m = MsgIn::new(payload);
        let (Ok(kind), Ok(name)) = (m.u8(), m.cstr()) else {
            return ext_err(
                &mut self.send,
                &mut self.phase,
                sqlstate::PROTOCOL_VIOLATION,
                "malformed Describe message",
            );
        };
        let mut portal_formats = ResultFmt::ALL_TEXT;
        let stmt_slot = match kind {
            b'S' => self
                .prepared
                .iter()
                .position(|p| p.active && p.name.as_str() == name),
            b'P' => self
                .portals
                .iter()
                .position(|p| p.active && p.name.as_str() == name)
                .map(|i| {
                    portal_formats = self.portals[i].result_formats;
                    self.portals[i].statement
                }),
            _ => {
                return ext_err(
                    &mut self.send,
                    &mut self.phase,
                    sqlstate::PROTOCOL_VIOLATION,
                    "Describe expects 'S' or 'P'",
                );
            }
        };
        let Some(slot) = stmt_slot else {
            let (code, what) = if kind == b'S' {
                ("26000", "prepared statement does not exist")
            } else {
                ("34000", "portal does not exist")
            };
            return ext_err(&mut self.send, &mut self.phase, code, what);
        };

        self.arena.reset();
        let n_params = self.prepared[slot].n_params;
        // Statement Describe: resolve each parameter's type from its use so the
        // client encodes arguments correctly, and remember it for Bind decoding.
        if kind == b'S' {
            let inferred = {
                let text = core::str::from_utf8(self.prepared[slot].text.readable())
                    .expect("stored from valid UTF-8");
                let client = self.prepared[slot].param_oids;
                engine.infer_param_types(text, &self.arena, &self.txn, &client)
            };
            self.prepared[slot].param_oids = inferred;
        }
        let param_oids = self.prepared[slot].param_oids;
        let text = core::str::from_utf8(self.prepared[slot].text.readable())
            .expect("stored from valid UTF-8");
        let mut responder = Responder::for_describe(&mut self.send, portal_formats);
        if kind == b'S'
            && responder
                .parameter_description(&param_oids[..n_params as usize])
                .is_err()
        {
            return Step::Close;
        }
        match engine.describe(text, &self.arena, &self.txn, &mut responder) {
            Ok(true) => Step::Continue,
            Ok(false) => {
                self.phase = Phase::SkipToSync;
                Step::Continue
            }
            Err(WireFull) => Step::Close,
        }
    }

    fn handle_execute(&mut self, engine: &mut Engine, total: usize) -> Step {
        let payload = &self.recv.readable()[5..total];
        let mut m = MsgIn::new(payload);
        let (Ok(name), Ok(max_rows)) = (m.cstr(), m.i32()) else {
            return ext_err(
                &mut self.send,
                &mut self.phase,
                sqlstate::PROTOCOL_VIOLATION,
                "malformed Execute message",
            );
        };
        let Some(portal_slot) = self
            .portals
            .iter()
            .position(|p| p.active && p.name.as_str() == name)
        else {
            return ext_err(
                &mut self.send,
                &mut self.phase,
                "34000",
                "portal does not exist",
            );
        };

        // A portal already producing rows (executed==true) always drains
        // from its buffer, even for a following Execute with max_rows=0.
        // A fresh portal buffers when paged (max_rows>0), else streams.
        let already_started = self.portals[portal_slot].executed;
        let mut paged = max_rows > 0 || already_started;
        let need_run = !already_started;

        if need_run {
            self.arena.reset();
            let lock_timeout_expired = self.lock_timeout_expired();
            let portal = &mut self.portals[portal_slot];
            let prepared = &self.prepared[portal.statement];
            if !prepared.active {
                return ext_err(
                    &mut self.send,
                    &mut self.phase,
                    "26000",
                    "prepared statement no longer exists",
                );
            }
            let text =
                core::str::from_utf8(prepared.text.readable()).expect("stored from valid UTF-8");
            if engine.is_copy_statement(text, &self.arena) {
                // COPY uses CopyData framing, not DataRow/PortalSuspended.
                // Stream it even when Execute carries a nonzero max_rows.
                paged = false;
                self.arena.reset();
            }
            let mut params = [Datum::Null; MAX_BIND_PARAMS];
            let raw = portal.params.readable();
            // Resolve any parameter the client left untyped (OID 0) from its use
            // in the query, so a binary-format value decodes as its real type
            // even without a prior statement Describe — e.g. an empty range,
            // which the client cannot subtype and so sends untyped. Mirrors what
            // Describe already does; text params are unaffected.
            let mut param_oids = prepared.param_oids;
            let has_untyped_binary =
                (0..portal.n_params as usize).any(|i| portal.binary[i] && param_oids[i] == 0);
            if has_untyped_binary {
                param_oids =
                    engine.infer_param_types(text, &self.arena, &self.txn, &prepared.param_oids);
            }
            for (i, &(offset, len)) in portal
                .spans
                .iter()
                .take(portal.n_params as usize)
                .enumerate()
            {
                if len == u32::MAX {
                    params[i] = Datum::Null;
                    continue;
                }
                let bytes = &raw[offset as usize..(offset + len) as usize];
                if portal.binary[i] {
                    match decode_binary_param(param_oids[i], bytes, &self.arena) {
                        Ok(v) => params[i] = v,
                        Err(message) => {
                            return ext_err(
                                &mut self.send,
                                &mut self.phase,
                                sqlstate::FEATURE_NOT_SUPPORTED,
                                message,
                            );
                        }
                    }
                } else {
                    params[i] = Datum::Text(unsafe { core::str::from_utf8_unchecked(bytes) });
                }
            }

            // Paged execution goes through the portal's result buffer so
            // later Execute messages can continue draining it.
            let rfmt = portal.result_formats;
            let send_mark = self.send.mark();
            let result = if paged {
                portal.result.clear();
                let mut responder = Responder::for_execute(&mut portal.result, rfmt);
                engine.execute_extended(
                    text,
                    &self.arena,
                    &params[..portal.n_params as usize],
                    &mut self.txn,
                    &mut self.sqlprep,
                    &mut self.cursors,
                    &mut self.guc,
                    &mut responder,
                    self.id,
                    lock_timeout_expired,
                )
            } else {
                let mut responder = Responder::for_execute(&mut self.send, rfmt);
                engine.execute_extended(
                    text,
                    &self.arena,
                    &params[..portal.n_params as usize],
                    &mut self.txn,
                    &mut self.sqlprep,
                    &mut self.cursors,
                    &mut self.guc,
                    &mut responder,
                    self.id,
                    lock_timeout_expired,
                )
            };
            engine.maybe_checkpoint();
            let pending_copy = engine.take_pending_copy();
            self.arena.reset();
            match result {
                Ok(crate::sql::ExtendedExecutionStatus::Complete(true)) => {
                    self.finish_lock_wait();
                }
                Ok(crate::sql::ExtendedExecutionStatus::Complete(false)) => {
                    self.finish_lock_wait();
                    if pending_copy.is_some() {
                        engine.copy_abort(&mut self.txn, &self.guc);
                    }
                    if paged {
                        // Forward the buffered error output.
                        let portal = &mut self.portals[portal_slot];
                        let bytes_ok = self.send.append(portal.result.readable());
                        portal.result.clear();
                        if !bytes_ok {
                            return Step::Close;
                        }
                    }
                    self.phase = Phase::SkipToSync;
                    return Step::Continue;
                }
                Ok(crate::sql::ExtendedExecutionStatus::Blocked { io_wait }) => {
                    if paged {
                        self.portals[portal_slot].result.clear();
                    } else {
                        self.send.truncate_to(send_mark);
                    }
                    let generation = engine.lock_generation();
                    self.park(io_wait, generation);
                    return Step::Parked;
                }
                Err(WireFull) => {
                    if pending_copy.is_some() {
                        engine.copy_abort(&mut self.txn, &self.guc);
                    }
                    return Step::Close;
                }
            }
            if let Some(setup) = pending_copy {
                if paged {
                    let portal = &mut self.portals[portal_slot];
                    let bytes_ok = self.send.append(portal.result.readable());
                    portal.result.clear();
                    if !bytes_ok {
                        engine.copy_abort(&mut self.txn, &self.guc);
                        return Step::Close;
                    }
                }
                let header_pending = setup.fmt.header;
                let binary_header_pending = setup.fmt.binary;
                self.copy = Some(CopyInProgress {
                    setup,
                    count: 0,
                    failed: None,
                    end_seen: false,
                    header_pending,
                    binary_header_pending,
                    extended: true,
                });
                self.copy_buf.clear();
                return Step::Continue;
            }
            if paged {
                self.portals[portal_slot].executed = true;
            } else {
                return Step::Continue;
            }
        }

        // Drain up to max_rows DataRow messages from the portal buffer.
        let portal = &mut self.portals[portal_slot];
        let mut sent = 0i32;
        loop {
            let data = portal.result.readable();
            if data.len() < 5 {
                break;
            }
            let msg_type = data[0];
            let len = i32::from_be_bytes(data[1..5].try_into().unwrap()) as usize;
            let total_msg = 1 + len;
            if data.len() < total_msg {
                break;
            }
            if msg_type == wire::MSG_DATA_ROW && max_rows > 0 && sent >= max_rows {
                // More rows remain: suspend the portal.
                let mut responder = Responder::new(&mut self.send);
                return match resp_portal_suspended(&mut responder) {
                    Ok(()) => Step::Continue,
                    Err(WireFull) => Step::Close,
                };
            }
            if !self.send.append(&data[..total_msg]) {
                return Step::Close;
            }
            if msg_type == wire::MSG_DATA_ROW {
                sent += 1;
            }
            portal.result.consume(total_msg);
        }
        Step::Continue
    }

    fn handle_close(&mut self, total: usize) -> Step {
        let payload = &self.recv.readable()[5..total];
        let mut m = MsgIn::new(payload);
        let (Ok(kind), Ok(name)) = (m.u8(), m.cstr()) else {
            return ext_err(
                &mut self.send,
                &mut self.phase,
                sqlstate::PROTOCOL_VIOLATION,
                "malformed Close message",
            );
        };
        match kind {
            b'S' => {
                if let Some(i) = self
                    .prepared
                    .iter()
                    .position(|p| p.active && p.name.as_str() == name)
                {
                    self.prepared[i].active = false;
                }
            }
            b'P' => {
                if let Some(i) = self
                    .portals
                    .iter()
                    .position(|p| p.active && p.name.as_str() == name)
                {
                    self.portals[i].active = false;
                }
            }
            _ => {
                return ext_err(
                    &mut self.send,
                    &mut self.phase,
                    sqlstate::PROTOCOL_VIOLATION,
                    "Close expects 'S' or 'P'",
                );
            }
        }
        let mut responder = Responder::new(&mut self.send);
        match responder.close_complete() {
            Ok(()) => Step::Continue,
            Err(WireFull) => Step::Close,
        }
    }

    fn handle_query(&mut self, engine: &mut Engine, total: usize) -> Step {
        // The query text borrows recv, and execution writes into send and
        // allocates from the arena — all disjoint fields.
        let payload = &self.recv.readable()[5..total];
        let Ok(text) = MsgIn::new(payload).cstr() else {
            let mut responder = Responder::new(&mut self.send);
            let _ = responder.error(sqlstate::PROTOCOL_VIOLATION, "malformed Query message");
            return Step::Close;
        };
        let identify_system = is_identify_system(text);
        if is_replication_command(self.replication, text) {
            return self.handle_replication_query(engine, identify_system, total);
        }
        self.arena.reset();
        let mark = self.send.mark();
        // Stream large results: put the socket in blocking mode so the
        // Responder can drain a full send buffer straight to the client and
        // continue, instead of failing with 54000. Restored afterward.
        let fd = self.stream.as_ref().map(|s| s.as_raw_fd());
        if let Some(stream) = self.stream.as_ref() {
            let _ = stream.set_nonblocking(false);
        }
        let result = {
            let lock_timeout_expired = self.lock_timeout_expired();
            let mut responder = Responder::new(&mut self.send);
            // Over TLS the drain must encrypt through the session onto the
            // blocking socket; in the clear it writes the fd directly.
            if let Some(session) = self.tls.as_mut() {
                responder = responder.with_flush_tls(session, self.stream.as_mut().unwrap());
            } else if let Some(fd) = fd {
                responder = responder.with_flush(fd);
            }
            engine.execute_simple_from(
                text,
                self.resume_statement,
                &self.arena,
                &mut self.txn,
                &mut self.sqlprep,
                &mut self.cursors,
                &mut self.guc,
                &mut responder,
                self.id,
                lock_timeout_expired,
            )
        };
        if let Some(stream) = self.stream.as_ref() {
            let _ = stream.set_nonblocking(true);
        }
        // Transactions fsync at commit; only checkpoint housekeeping
        // remains here (safe while transactions are open: it snapshots
        // committed state only).
        engine.maybe_checkpoint();
        let status = self.txn.status_byte();
        let step = match result {
            Ok(crate::sql::ExecutionStatus::Complete) => {
                self.finish_lock_wait();
                self.resume_statement = 0;
                // A COPY FROM STDIN holds its query cycle open: the
                // connection enters copy-in mode and ReadyForQuery waits
                // for CopyDone.
                if let Some(setup) = engine.take_pending_copy() {
                    let header_pending = setup.fmt.header;
                    let binary_header_pending = setup.fmt.binary;
                    self.copy = Some(CopyInProgress {
                        setup,
                        count: 0,
                        failed: None,
                        end_seen: false,
                        header_pending,
                        binary_header_pending,
                        extended: false,
                    });
                    self.copy_buf.clear();
                    self.recv.consume(total);
                    return Step::Continue;
                }
                let mut responder = Responder::new(&mut self.send);
                match responder.ready_for_query(status) {
                    Ok(()) => Step::Continue,
                    Err(WireFull) => Step::Close,
                }
            }
            Ok(crate::sql::ExecutionStatus::Blocked {
                completed_statements,
                output_mark,
                io_wait,
            }) => {
                self.send.truncate_to(output_mark);
                self.resume_statement = completed_statements;
                let generation = engine.lock_generation();
                self.park(io_wait, generation);
                self.arena.reset();
                return Step::Parked;
            }
            Err(WireFull) => {
                if engine.take_pending_copy().is_some() {
                    engine.copy_abort(&mut self.txn, &self.guc);
                }
                let mut responder = Responder::new(&mut self.send);
                let recovered = responder
                    .replace_with_overflow_error(mark)
                    .and_then(|()| responder.ready_for_query(status));
                match recovered {
                    Ok(()) => Step::Continue,
                    Err(WireFull) => Step::Close,
                }
            }
        };
        self.arena.reset();
        self.recv.consume(total);
        step
    }

    fn handle_replication_query(
        &mut self,
        engine: &mut Engine,
        identify_system: bool,
        total: usize,
    ) -> Step {
        if identify_system {
            let (system_id, lsn) = engine.replication_identity();
            let system_id = stack_format!(32, "{system_id}");
            let lsn = stack_format!(32, "0/{lsn:X}");
            let columns = [
                crate::sql::types::ColDesc::new("systemid", crate::sql::types::oid::TEXT, -1),
                crate::sql::types::ColDesc::new("timeline", crate::sql::types::oid::INT8, 8),
                crate::sql::types::ColDesc::new("xlogpos", crate::sql::types::oid::TEXT, -1),
                crate::sql::types::ColDesc::new("dbname", crate::sql::types::oid::TEXT, -1),
            ];
            let values = [
                Datum::Text(system_id.as_str()),
                Datum::Int8(1),
                Datum::Text(lsn.as_str()),
                if self.replication == ReplicationMode::Logical {
                    Datum::Text("postgres")
                } else {
                    Datum::Null
                },
            ];
            let mut responder = Responder::new(&mut self.send);
            if responder
                .row_description(&columns)
                .and_then(|()| responder.data_row(&values))
                .and_then(|()| responder.command_complete("IDENTIFY_SYSTEM"))
                .and_then(|()| responder.ready_for_query(b'I'))
                .is_err()
            {
                return Step::Close;
            }
            self.recv.consume(total);
            return Step::Continue;
        }
        if self.replication == ReplicationMode::Logical {
            let command = {
                let payload = &self.recv.readable()[5..total];
                let Ok(text) = MsgIn::new(payload).cstr() else {
                    let mut responder = Responder::new(&mut self.send);
                    let _ =
                        responder.error(sqlstate::PROTOCOL_VIOLATION, "malformed Query message");
                    return Step::Close;
                };
                parse_logical_replication_command(text)
            };
            match command {
                Ok(LogicalReplicationCommand::CreateSlot { name }) => {
                    let restart_lsn = match engine.create_replication_slot(name) {
                        Ok(lsn) => lsn,
                        Err(error) => {
                            let mut responder = Responder::new(&mut self.send);
                            if responder
                                .error(error.sqlstate, error.message.as_str())
                                .and_then(|()| responder.ready_for_query(b'I'))
                                .is_err()
                            {
                                return Step::Close;
                            }
                            self.recv.consume(total);
                            return Step::Continue;
                        }
                    };
                    let lsn = stack_format!(32, "0/{restart_lsn:X}");
                    let columns = [
                        crate::sql::types::ColDesc::new(
                            "slot_name",
                            crate::sql::types::oid::TEXT,
                            -1,
                        ),
                        crate::sql::types::ColDesc::new(
                            "consistent_point",
                            crate::sql::types::oid::TEXT,
                            -1,
                        ),
                        crate::sql::types::ColDesc::new(
                            "snapshot_name",
                            crate::sql::types::oid::TEXT,
                            -1,
                        ),
                        crate::sql::types::ColDesc::new(
                            "output_plugin",
                            crate::sql::types::oid::TEXT,
                            -1,
                        ),
                    ];
                    let values = [
                        Datum::Text(name.as_str()),
                        Datum::Text(lsn.as_str()),
                        Datum::Null,
                        Datum::Text("pgoutput"),
                    ];
                    let mut responder = Responder::new(&mut self.send);
                    if responder
                        .row_description(&columns)
                        .and_then(|()| responder.data_row(&values))
                        .and_then(|()| responder.command_complete("CREATE_REPLICATION_SLOT"))
                        .and_then(|()| responder.ready_for_query(b'I'))
                        .is_err()
                    {
                        return Step::Close;
                    }
                    self.recv.consume(total);
                    return Step::Continue;
                }
                Ok(LogicalReplicationCommand::DropSlot { name }) => {
                    if let Err(error) = engine.drop_replication_slot(name) {
                        let mut responder = Responder::new(&mut self.send);
                        if responder
                            .error(error.sqlstate, error.message.as_str())
                            .and_then(|()| responder.ready_for_query(b'I'))
                            .is_err()
                        {
                            return Step::Close;
                        }
                        self.recv.consume(total);
                        return Step::Continue;
                    }
                    let mut responder = Responder::new(&mut self.send);
                    if responder
                        .command_complete("DROP_REPLICATION_SLOT")
                        .and_then(|()| responder.ready_for_query(b'I'))
                        .is_err()
                    {
                        return Step::Close;
                    }
                    self.recv.consume(total);
                    return Step::Continue;
                }
                Ok(LogicalReplicationCommand::Start {
                    name,
                    publication,
                    requested_lsn,
                    binary,
                    proto_version,
                }) => {
                    self.replication_publications.clear();
                    if let Err(error) =
                        parse_publication_names(publication, &mut self.replication_publications)
                    {
                        let mut responder = Responder::new(&mut self.send);
                        let _ = responder
                            .error(error.sqlstate, error.message.as_str())
                            .and_then(|()| responder.ready_for_query(b'I'));
                        self.recv.consume(total);
                        return Step::Continue;
                    }
                    let cursor_lsn = match engine.activate_replication_slot(name.as_str()) {
                        Ok(lsn) => lsn,
                        Err(error) => {
                            let mut responder = Responder::new(&mut self.send);
                            let _ = responder
                                .error(error.sqlstate, error.message.as_str())
                                .and_then(|()| responder.ready_for_query(b'I'));
                            self.recv.consume(total);
                            return Step::Continue;
                        }
                    };
                    let mut responder = Responder::new(&mut self.send);
                    if responder.copy_both_response().is_err() {
                        engine.deactivate_replication_slot(name.as_str());
                        return Step::Close;
                    }
                    self.replication_stream = Some(ReplicationStream {
                        slot: name,
                        binary,
                        proto_version,
                        cursor_lsn,
                        scan_lsn: cursor_lsn.max(requested_lsn),
                        last_sent_lsn: cursor_lsn.max(requested_lsn),
                        last_message_at: Instant::now(),
                        reply_requested: false,
                    });
                    if matches!(self.pump_replication(engine), After::Close) {
                        self.stop_replication(engine);
                        return Step::Close;
                    }
                    self.recv.consume(total);
                    return Step::Continue;
                }
                Err(error) => {
                    let mut responder = Responder::new(&mut self.send);
                    if responder
                        .error(error.sqlstate, error.message.as_str())
                        .and_then(|()| responder.ready_for_query(b'I'))
                        .is_err()
                    {
                        return Step::Close;
                    }
                    self.recv.consume(total);
                    return Step::Continue;
                }
            }
        }
        let message = match self.replication {
            ReplicationMode::Physical => "physical replication is not supported",
            ReplicationMode::Logical => "logical replication commands are not implemented yet",
            ReplicationMode::None => unreachable!("only replication connections reach this path"),
        };
        let mut responder = Responder::new(&mut self.send);
        if responder
            .error(sqlstate::FEATURE_NOT_SUPPORTED, message)
            .and_then(|()| responder.ready_for_query(b'I'))
            .is_err()
        {
            return Step::Close;
        }
        self.recv.consume(total);
        Step::Continue
    }

    /// Writes as much of the send buffer as the socket accepts.
    /// `Err` means the connection is broken.
    fn flush(&mut self) -> Result<(), ()> {
        if self.stream.is_none() {
            return Err(());
        }
        if let Some(tls) = self.tls.as_mut() {
            let socket = self.stream.as_mut().unwrap();
            // Hand the plaintext to the session (it buffers into records), then
            // push as much ciphertext to the socket as it accepts. Leftover
            // ciphertext stays queued in rustls; `wants_write` keeps write
            // interest registered until it drains.
            while !self.send.is_empty() {
                match tls.queue(self.send.readable()) {
                    Ok(0) => return Err(()),
                    Ok(n) => self.send.consume(n),
                    Err(_) => return Err(()),
                }
            }
            return match tls.flush_nonblocking(socket) {
                Ok(()) => Ok(()),
                Err(_) => Err(()),
            };
        }
        let stream = self.stream.as_mut().unwrap();
        while !self.send.is_empty() {
            match stream.write(self.send.readable()) {
                Ok(0) => return Err(()),
                Ok(n) => self.send.consume(n),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return Err(()),
            }
        }
        Ok(())
    }
}

fn is_identify_system(text: &str) -> bool {
    text.trim()
        .trim_end_matches(';')
        .trim_end()
        .eq_ignore_ascii_case("identify_system")
}

/// Logical replication uses replication commands and ordinary SQL on the same
/// simple-query connection. Physical mode has no SQL database session, so all
/// of its simple queries are replication commands.
fn is_replication_command(mode: ReplicationMode, text: &str) -> bool {
    mode == ReplicationMode::Physical
        || (mode == ReplicationMode::Logical
            && (is_identify_system(text)
                || text
                    .trim_start()
                    .get(..17)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("start_replication"))
                || text
                    .trim_start()
                    .get(..23)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("create_replication_slot"))
                || text
                    .trim_start()
                    .get(..21)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("drop_replication_slot"))))
}

enum LogicalReplicationCommand<'a> {
    CreateSlot {
        name: SqlName,
    },
    DropSlot {
        name: SqlName,
    },
    Start {
        name: SqlName,
        publication: &'a str,
        requested_lsn: u64,
        binary: bool,
        proto_version: u8,
    },
}

fn parse_logical_replication_command(
    text: &str,
) -> Result<LogicalReplicationCommand<'_>, SqlError> {
    let text = text.trim().trim_end_matches(';').trim_end();
    let mut input = text;
    let command = take_replication_word(&mut input).unwrap_or_default();
    if command.eq_ignore_ascii_case("drop_replication_slot") {
        let name = take_replication_word(&mut input).ok_or_else(|| {
            sql_err!(
                sqlstate::SYNTAX_ERROR,
                "DROP_REPLICATION_SLOT requires a slot name"
            )
        })?;
        if !is_replication_slot_name(name) || take_replication_word(&mut input).is_some() {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "invalid DROP_REPLICATION_SLOT input"
            ));
        }
        return Ok(LogicalReplicationCommand::DropSlot {
            name: SqlName::parse(name)?,
        });
    }
    if command.eq_ignore_ascii_case("start_replication") {
        let slot_keyword = take_replication_word(&mut input)
            .ok_or_else(|| sql_err!(sqlstate::SYNTAX_ERROR, "START_REPLICATION requires SLOT"))?;
        let name = take_replication_word(&mut input).ok_or_else(|| {
            sql_err!(
                sqlstate::SYNTAX_ERROR,
                "START_REPLICATION requires a slot name"
            )
        })?;
        let logical = take_replication_word(&mut input).ok_or_else(|| {
            sql_err!(sqlstate::SYNTAX_ERROR, "START_REPLICATION requires LOGICAL")
        })?;
        let lsn = take_replication_word(&mut input)
            .ok_or_else(|| sql_err!(sqlstate::SYNTAX_ERROR, "START_REPLICATION requires an LSN"))?;
        if !slot_keyword.eq_ignore_ascii_case("slot")
            || !logical.eq_ignore_ascii_case("logical")
            || !is_replication_slot_name(name)
        {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "invalid START_REPLICATION input"
            ));
        }
        let requested_lsn = parse_lsn(lsn)
            .ok_or_else(|| sql_err!(sqlstate::SYNTAX_ERROR, "invalid START_REPLICATION LSN"))?;
        let (publication, binary, proto_version) = parse_pgoutput_options(input)?;
        return Ok(LogicalReplicationCommand::Start {
            name: SqlName::parse(name)?,
            publication,
            requested_lsn,
            binary,
            proto_version,
        });
    }
    if !command.eq_ignore_ascii_case("create_replication_slot") {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "expected CREATE_REPLICATION_SLOT"
        ));
    }
    let name = take_replication_word(&mut input).ok_or_else(|| {
        sql_err!(
            sqlstate::SYNTAX_ERROR,
            "CREATE_REPLICATION_SLOT requires a slot name"
        )
    })?;
    if !is_replication_slot_name(name) {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "invalid replication slot name"
        ));
    }
    let kind = take_replication_word(&mut input).ok_or_else(|| {
        sql_err!(
            sqlstate::SYNTAX_ERROR,
            "CREATE_REPLICATION_SLOT requires LOGICAL pgoutput"
        )
    })?;
    let plugin = take_replication_word(&mut input).ok_or_else(|| {
        sql_err!(
            sqlstate::SYNTAX_ERROR,
            "CREATE_REPLICATION_SLOT requires LOGICAL pgoutput"
        )
    })?;
    if !kind.eq_ignore_ascii_case("logical") || !plugin.eq_ignore_ascii_case("pgoutput") {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "only LOGICAL pgoutput replication slots are supported"
        ));
    }
    if take_replication_word(&mut input).is_some() {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "trailing CREATE_REPLICATION_SLOT input"
        ));
    }
    Ok(LogicalReplicationCommand::CreateSlot {
        name: SqlName::parse(name)?,
    })
}

fn take_replication_word<'a>(input: &mut &'a str) -> Option<&'a str> {
    *input = input.trim_start_matches(char::is_whitespace);
    if input.is_empty() {
        return None;
    }
    let end = input.find(char::is_whitespace).unwrap_or(input.len());
    let (word, rest) = input.split_at(end);
    *input = rest;
    Some(word)
}

fn parse_lsn(value: &str) -> Option<u64> {
    let (high, low) = value.split_once('/')?;
    if !high.is_empty()
        && !low.is_empty()
        && high.len() <= 8
        && low.len() <= 8
        && high.bytes().all(|byte| byte.is_ascii_hexdigit())
        && low.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        let high = u32::from_str_radix(high, 16).ok()?;
        let low = u32::from_str_radix(low, 16).ok()?;
        Some((u64::from(high) << 32) | u64::from(low))
    } else {
        None
    }
}

/// Parses pgoutput's parenthesized option list without accepting ignored
/// keys. Values are SQL-style single-quoted strings; the bounded publication
/// name is copied only after every option has been validated.
fn parse_pgoutput_options(input: &str) -> Result<(&str, bool, u8), SqlError> {
    let mut input = input.trim();
    let Some(body) = input.strip_prefix('(') else {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "START_REPLICATION requires pgoutput options"
        ));
    };
    input = body;
    let mut publication = None;
    let mut proto_version = None;
    let mut binary = false;
    let mut saw_binary = false;
    loop {
        input = input.trim_start();
        if let Some(rest) = input.strip_prefix(')') {
            if !rest.trim().is_empty() {
                return Err(sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "trailing START_REPLICATION input"
                ));
            }
            break;
        }
        let key_end = input
            .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .unwrap_or(input.len());
        if key_end == 0 {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "invalid pgoutput option name"
            ));
        }
        let key = &input[..key_end];
        input = input[key_end..].trim_start();
        if let Some(rest) = input.strip_prefix('=') {
            input = rest.trim_start();
        }
        let Some(rest) = input.strip_prefix('\'') else {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "pgoutput option values must be quoted"
            ));
        };
        let Some(end) = rest.find('\'') else {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "unterminated pgoutput option value"
            ));
        };
        let value = &rest[..end];
        input = rest[end + 1..].trim_start();
        if let Some(rest) = input.strip_prefix(',') {
            input = rest;
        } else if !input.starts_with(')') {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "pgoutput options require commas"
            ));
        }
        if key.eq_ignore_ascii_case("proto_version") {
            if proto_version.replace(value).is_some() {
                return Err(sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "duplicate pgoutput proto_version"
                ));
            }
        } else if key.eq_ignore_ascii_case("publication_names") {
            if publication.replace(value).is_some() {
                return Err(sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "duplicate pgoutput publication_names"
                ));
            }
        } else if key.eq_ignore_ascii_case("binary") {
            if saw_binary {
                return Err(sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "duplicate pgoutput binary option"
                ));
            }
            binary = match value {
                "true" => true,
                "false" => false,
                _ => {
                    return Err(sql_err!(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        "pgoutput binary must be 'true' or 'false'"
                    ));
                }
            };
            saw_binary = true;
        } else {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "unsupported pgoutput option \"{}\"",
                key
            ));
        }
    }
    let proto_version = match proto_version {
        Some("1") => 1,
        Some("2") => 2,
        _ => {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "pgoutput requires proto_version '1' or '2'"
            ));
        }
    };
    let publication = publication.ok_or_else(|| {
        sql_err!(
            sqlstate::SYNTAX_ERROR,
            "START_REPLICATION requires publication_names"
        )
    })?;
    if publication.trim().is_empty() {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "START_REPLICATION requires publication_names"
        ));
    }
    if publication.trim_end().ends_with(',') {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "publication_names contains an empty name"
        ));
    }
    Ok((publication, binary, proto_version))
}

fn parse_publication_names(
    input: &str,
    publications: &mut FixedVec<SqlName>,
) -> Result<(), SqlError> {
    use core::fmt::Write;

    let mut input = input.trim();
    if input.is_empty() {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "START_REPLICATION requires publication_names"
        ));
    }
    loop {
        input = input.trim_start();
        if input.is_empty() {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "publication_names contains an empty name"
            ));
        }
        let mut name = StackStr::<64>::new();
        let rest = if let Some(quoted) = input.strip_prefix('"') {
            let mut quoted = quoted;
            loop {
                let Some(character) = quoted.chars().next() else {
                    return Err(sql_err!(
                        sqlstate::SYNTAX_ERROR,
                        "unterminated quoted publication name"
                    ));
                };
                quoted = &quoted[character.len_utf8()..];
                if character == '"' {
                    if let Some(rest) = quoted.strip_prefix('"') {
                        quoted = rest;
                        let _ = name.write_char('"');
                        continue;
                    }
                    break quoted;
                }
                let _ = name.write_char(character);
            }
        } else {
            let end = input.find(',').unwrap_or(input.len());
            let token = input[..end].trim_end();
            if token.is_empty()
                || !token.chars().enumerate().all(|(index, character)| {
                    if index == 0 {
                        character == '_' || character.is_alphabetic() || !character.is_ascii()
                    } else {
                        character == '_'
                            || character == '$'
                            || character.is_alphanumeric()
                            || !character.is_ascii()
                    }
                })
            {
                return Err(sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "invalid unquoted publication name"
                ));
            }
            for character in token.chars() {
                let _ = name.write_char(character.to_ascii_lowercase());
            }
            &input[end..]
        };
        if name.as_str().is_empty() {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "publication_names contains an empty name"
            ));
        }
        if name.is_truncated() {
            return Err(sql_err!(
                sqlstate::NAME_TOO_LONG,
                "publication name is longer than 63 bytes"
            ));
        }
        publications
            .push(SqlName::parse(name.as_str())?)
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "publication_names exceeds configured publication capacity"
                )
            })?;
        let rest = rest.trim_start();
        if rest.is_empty() {
            return Ok(());
        }
        let Some(rest) = rest.strip_prefix(',') else {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "publication_names require commas"
            ));
        };
        input = rest;
    }
}

fn is_replication_slot_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

enum Step {
    Continue,
    NeedMoreData,
    Parked,
    Close,
}

/// The connection half of a COPY FROM STDIN: the engine's setup, the rows
/// stored so far, the first error (data after it drains unparsed, as
/// PostgreSQL does), and whether the classic `\.` end marker was seen.
struct CopyInProgress {
    setup: crate::sql::exec::CopySetup,
    count: u64,
    failed: Option<crate::sql::eval::SqlError>,
    end_seen: bool,
    /// CSV/text HEADER: the first data line is column names to skip, not a row.
    header_pending: bool,
    /// Binary format: the file header (signature + flags) is still to consume.
    binary_header_pending: bool,
    /// Extended-query COPY completes with CommandComplete/ErrorResponse only;
    /// ReadyForQuery belongs to a later Sync. Simple-query COPY sends it here.
    extended: bool,
}

fn resp_portal_suspended(responder: &mut Responder) -> Result<(), crate::pg::wire::WireFull> {
    use crate::pg::wire::{MSG_PORTAL_SUSPENDED, MsgOut};
    MsgOut::begin(responder.buffer, MSG_PORTAL_SUSPENDED).finish()
}

/// Decodes a binary-format parameter using its declared type OID
/// (network byte order per the protocol's binary representations). `arena`
/// backs the values (e.g. NUMERIC) that need it.
pub(crate) fn decode_binary_param<'a>(
    oid: i32,
    bytes: &'a [u8],
    arena: &'a crate::mem::arena::Arena,
) -> Result<Datum<'a>, &'static str> {
    use crate::sql::types::oid as oids;
    let wrong = "binary parameter length does not match its type";
    match oid {
        oids::BOOL => {
            let b: [u8; 1] = bytes.try_into().map_err(|_| wrong)?;
            Ok(Datum::Bool(b[0] != 0))
        }
        oids::INT2 => {
            let b: [u8; 2] = bytes.try_into().map_err(|_| wrong)?;
            Ok(Datum::Int4(i32::from(i16::from_be_bytes(b))))
        }
        oids::INT4 => {
            let b: [u8; 4] = bytes.try_into().map_err(|_| wrong)?;
            Ok(Datum::Int4(i32::from_be_bytes(b)))
        }
        oids::INT8 => {
            let b: [u8; 8] = bytes.try_into().map_err(|_| wrong)?;
            Ok(Datum::Int8(i64::from_be_bytes(b)))
        }
        oids::FLOAT4 => {
            let b: [u8; 4] = bytes.try_into().map_err(|_| wrong)?;
            Ok(Datum::Float4(f32::from_be_bytes(b)))
        }
        oids::FLOAT8 => {
            let b: [u8; 8] = bytes.try_into().map_err(|_| wrong)?;
            Ok(Datum::Float8(f64::from_be_bytes(b)))
        }
        oids::TEXT | oids::VARCHAR | 0 => core::str::from_utf8(bytes)
            .map(Datum::Text)
            .map_err(|_| "invalid UTF-8 in binary text parameter"),
        oids::DATE => {
            let b: [u8; 4] = bytes.try_into().map_err(|_| wrong)?;
            Ok(Datum::Date(i32::from_be_bytes(b)))
        }
        oids::TIMESTAMP => {
            let b: [u8; 8] = bytes.try_into().map_err(|_| wrong)?;
            Ok(Datum::Timestamp(i64::from_be_bytes(b)))
        }
        oids::TIMESTAMPTZ => {
            let b: [u8; 8] = bytes.try_into().map_err(|_| wrong)?;
            Ok(Datum::Timestamptz(i64::from_be_bytes(b)))
        }
        oids::UUID => {
            let b: [u8; 16] = bytes.try_into().map_err(|_| wrong)?;
            Ok(Datum::Uuid(b))
        }
        oids::BYTEA => Ok(Datum::Bytea(bytes)),
        oids::TIME => {
            let b: [u8; 8] = bytes.try_into().map_err(|_| wrong)?;
            Ok(Datum::Time(i64::from_be_bytes(b)))
        }
        oids::INTERVAL => {
            // 8-byte microseconds, 4-byte days, 4-byte months (all big-endian).
            let b: [u8; 16] = bytes.try_into().map_err(|_| wrong)?;
            let micros = i64::from_be_bytes(b[0..8].try_into().unwrap());
            let days = i32::from_be_bytes(b[8..12].try_into().unwrap());
            let months = i32::from_be_bytes(b[12..16].try_into().unwrap());
            Ok(Datum::Interval(crate::sql::types::Interval {
                months,
                days,
                micros,
            }))
        }
        oids::JSON => core::str::from_utf8(bytes)
            .map(|t| Datum::Json {
                text: t,
                jsonb: false,
            })
            .map_err(|_| "invalid UTF-8 in binary json parameter"),
        oids::JSONB => {
            // jsonb send format: a 1-byte version (0x01) then the JSON text.
            let (&ver, rest) = bytes.split_first().ok_or(wrong)?;
            if ver != 1 {
                return Err("unsupported jsonb binary version");
            }
            core::str::from_utf8(rest)
                .map(|t| Datum::Json {
                    text: t,
                    jsonb: true,
                })
                .map_err(|_| "invalid UTF-8 in binary jsonb parameter")
        }
        oids::NUMERIC => {
            let mut buffer = crate::util::StackStr::<96>::new();
            binary_numeric_to_str(bytes, &mut buffer)?;
            crate::sql::numeric::Numeric::parse(buffer.as_str(), arena)
                .map(Datum::Numeric)
                .map_err(|_| "binary numeric out of range")
        }
        oids::INET | oids::CIDR => {
            // family (2 = v4, 3 = v6), mask bits, is_cidr flag, address byte
            // count, then the address bytes.
            if bytes.len() < 4 {
                return Err("truncated binary inet/cidr parameter");
            }
            let nb = bytes[3] as usize;
            if (nb != 4 && nb != 16) || bytes.len() != 4 + nb {
                return Err("malformed binary inet/cidr parameter");
            }
            let mut addr = [0u8; 16];
            addr[..nb].copy_from_slice(&bytes[4..4 + nb]);
            let net = crate::sql::net::NetAddr {
                family: if bytes[0] == 2 { 4 } else { 6 },
                bits: bytes[1],
                addr,
            };
            Ok(if oid == oids::CIDR {
                Datum::Cidr(net)
            } else {
                Datum::Inet(net)
            })
        }
        oids::MACADDR => {
            let b: [u8; 6] = bytes.try_into().map_err(|_| wrong)?;
            Ok(Datum::Macaddr(b))
        }
        oids::MACADDR8 => {
            let b: [u8; 8] = bytes.try_into().map_err(|_| wrong)?;
            Ok(Datum::Macaddr8(b))
        }
        // Composite types (arrays, ranges, multiranges, bit strings) share the
        // COPY-binary receive codec, driven by the column type the OID names.
        _ => match crate::sql::types::ColType::from_oid(oid) {
            Some(
                ctype @ (crate::sql::types::ColType::Array(_)
                | crate::sql::types::ColType::Range(_)
                | crate::sql::types::ColType::Multirange(_)
                | crate::sql::types::ColType::Bit { .. }),
            ) => crate::sql::exec::decode_binary_field(ctype, bytes, arena)
                .map_err(|_| "invalid binary composite parameter"),
            _ => Err("binary format for this parameter type is not implemented (use text)"),
        },
    }
}

/// Renders a PostgreSQL binary NUMERIC (base-10000 digit groups) into its
/// decimal string form so the existing text parser can build the value.
fn binary_numeric_to_str(
    bytes: &[u8],
    out: &mut crate::util::StackStr<96>,
) -> Result<(), &'static str> {
    use core::fmt::Write as _;
    let wrong = "binary parameter length does not match its type";
    if bytes.len() < 8 {
        return Err(wrong);
    }
    let rd = |o: usize| i16::from_be_bytes([bytes[o], bytes[o + 1]]);
    let ndigits = rd(0) as usize;
    let weight = rd(2) as i32;
    let sign = rd(4) as u16;
    let dscale = rd(6).max(0) as usize;
    if bytes.len() != 8 + ndigits * 2 {
        return Err(wrong);
    }
    if sign == 0xC000 {
        let _ = out.write_str("NaN");
        return finish_numeric(out, wrong);
    }
    let digit = |i: i32| -> i16 {
        if i >= 0 && (i as usize) < ndigits {
            rd(8 + i as usize * 2)
        } else {
            0
        }
    };
    if sign == 0x4000 {
        let _ = out.write_char('-');
    }
    // Integer part: groups at weight..=0 (a leading group prints unpadded).
    if weight < 0 {
        let _ = out.write_char('0');
    } else {
        for i in 0..=weight {
            let d = digit(i);
            if i == 0 {
                let _ = write!(out, "{d}");
            } else {
                let _ = write!(out, "{d:04}");
            }
        }
    }
    // Fractional part: exactly `dscale` decimal digits from the groups past the
    // integer part.
    if dscale > 0 {
        let _ = out.write_char('.');
        let mut written = 0usize;
        let mut gi = weight + 1;
        while written < dscale {
            let d = digit(gi);
            // Each group is 4 decimal digits; emit only up to dscale.
            let take = (dscale - written).min(4);
            let group = alloc_group_digits(d);
            let s = core::str::from_utf8(&group[..take]).expect("ascii digits");
            let _ = out.write_str(s);
            written += take;
            gi += 1;
        }
    }
    finish_numeric(out, wrong)
}

fn finish_numeric(
    out: &crate::util::StackStr<96>,
    wrong: &'static str,
) -> Result<(), &'static str> {
    if out.is_truncated() {
        Err(wrong)
    } else {
        Ok(())
    }
}

/// The four decimal digits of one base-10000 group, zero-padded.
fn alloc_group_digits(d: i16) -> [u8; 4] {
    let v = d.clamp(0, 9999) as u16;
    [
        b'0' + (v / 1000) as u8,
        b'0' + (v / 100 % 10) as u8,
        b'0' + (v / 10 % 10) as u8,
        b'0' + (v % 10) as u8,
    ]
}

/// Writes an error and puts the connection into extended-protocol error
/// recovery (discard until Sync). Free function so callers can hold
/// borrows of other connection fields.
fn ext_err(send: &mut FixedBuf, phase: &mut Phase, code: &str, message: &str) -> Step {
    let mut responder = Responder::new(send);
    if responder.error(code, message).is_err() {
        return Step::Close;
    }
    *phase = Phase::SkipToSync;
    Step::Continue
}

#[cfg(test)]
mod tests {
    use super::*;

    fn num_str(bytes: &[u8]) -> String {
        let mut out = crate::util::StackStr::<96>::new();
        binary_numeric_to_str(bytes, &mut out).expect("decode");
        out.as_str().to_string()
    }

    #[test]
    fn binary_numeric_decoding() {
        // PostgreSQL binary numeric: i16 ndigits, weight, sign, dscale, then
        // base-10000 digit groups (big-endian). Values verified against PG 18.4.
        // 2.50 -> ndigits 2, weight 0, sign +, dscale 2, digits [2, 5000].
        assert_eq!(num_str(&[0, 2, 0, 0, 0, 0, 0, 2, 0, 2, 0x13, 0x88]), "2.50");
        // -0.50 -> ndigits 1, weight -1, sign 0x4000, dscale 2, digit [5000].
        assert_eq!(
            num_str(&[0, 1, 0xFF, 0xFF, 0x40, 0, 0, 2, 0x13, 0x88]),
            "-0.50"
        );
        // 12345 -> ndigits 2, weight 1, sign +, dscale 0, digits [1, 2345].
        assert_eq!(
            num_str(&[0, 2, 0, 1, 0, 0, 0, 0, 0, 1, 0x09, 0x29]),
            "12345"
        );
        // NaN -> sign 0xC000.
        assert_eq!(num_str(&[0, 0, 0, 0, 0xC0, 0, 0, 0]), "NaN");
    }

    #[test]
    fn trust_authentication_does_not_require_a_password_verifier() {
        let login = crate::sql::RoleLogin {
            slot: 0,
            can_login: true,
            valid: true,
            superuser: true,
            replication: true,
            connection_limit: -1,
            password: None,
        };
        assert!(!reject_role_login(AuthMode::Trust, login, false));
        assert!(reject_role_login(AuthMode::Password, login, false));
        assert!(reject_role_login(AuthMode::ScramSha256, login, false));
        assert!(!reject_role_login(AuthMode::Password, login, true));
    }

    #[test]
    fn replication_requires_a_replication_role_or_superuser() {
        let login = crate::sql::RoleLogin {
            slot: 0,
            can_login: true,
            valid: true,
            superuser: false,
            replication: false,
            connection_limit: -1,
            password: None,
        };
        assert!(reject_replication_login(ReplicationMode::Logical, login));
        assert!(reject_replication_login(ReplicationMode::Physical, login));
        assert!(!reject_replication_login(ReplicationMode::None, login));
        let replication_login = crate::sql::RoleLogin {
            replication: true,
            ..login
        };
        assert!(!reject_replication_login(
            ReplicationMode::Logical,
            replication_login
        ));
        let superuser_login = crate::sql::RoleLogin {
            superuser: true,
            ..login
        };
        assert!(!reject_replication_login(
            ReplicationMode::Physical,
            superuser_login
        ));
    }

    #[test]
    fn logical_slot_creation_command_is_strict_and_pgoutput_only() {
        let command =
            parse_logical_replication_command("CREATE_REPLICATION_SLOT changes LOGICAL pgoutput;")
                .unwrap();
        let LogicalReplicationCommand::CreateSlot { name } = command else {
            panic!("expected CREATE_REPLICATION_SLOT")
        };
        assert_eq!(name.as_str(), "changes");
        assert!(
            parse_logical_replication_command(
                "CREATE_REPLICATION_SLOT changes LOGICAL test_decoding"
            )
            .is_err()
        );
        assert!(parse_logical_replication_command("CREATE_REPLICATION_SLOT changes").is_err());
        assert!(
            parse_logical_replication_command(
                "CREATE_REPLICATION_SLOT changes LOGICAL pgoutput trailing"
            )
            .is_err()
        );
        assert!(
            parse_logical_replication_command("CREATE_REPLICATION_SLOT bad/name LOGICAL pgoutput")
                .is_err()
        );
        let dropped = parse_logical_replication_command("DROP_REPLICATION_SLOT changes").unwrap();
        let LogicalReplicationCommand::DropSlot { name } = dropped else {
            panic!("expected DROP_REPLICATION_SLOT")
        };
        assert_eq!(name.as_str(), "changes");
    }

    #[test]
    fn logical_start_replication_strictly_negotiates_pgoutput() {
        let command = parse_logical_replication_command(
            "START_REPLICATION SLOT changes LOGICAL 0/0 (proto_version '1', publication_names 'changes_pub')",
        )
        .unwrap();
        let LogicalReplicationCommand::Start {
            name,
            publication,
            requested_lsn,
            binary,
            proto_version,
        } = command
        else {
            panic!("expected START_REPLICATION")
        };
        assert_eq!(name.as_str(), "changes");
        assert_eq!(publication, "changes_pub");
        assert_eq!(requested_lsn, 0);
        assert!(!binary);
        assert_eq!(proto_version, 1);
        let command = parse_logical_replication_command(
            "START_REPLICATION SLOT changes LOGICAL 0/0 (publication_names 'changes_pub', binary 'true', proto_version '1')",
        )
        .unwrap();
        let LogicalReplicationCommand::Start { binary, .. } = command else {
            panic!("expected START_REPLICATION")
        };
        assert!(binary);
        let command = parse_logical_replication_command(
            "START_REPLICATION SLOT changes LOGICAL 0/0 (proto_version '2', publication_names 'changes_pub')",
        )
        .unwrap();
        let LogicalReplicationCommand::Start { proto_version, .. } = command else {
            panic!("expected START_REPLICATION")
        };
        assert_eq!(proto_version, 2);
        let command = parse_logical_replication_command(
            "START_REPLICATION SLOT changes LOGICAL 1/2 (proto_version '1', publication_names 'changes_pub')",
        )
        .unwrap();
        let LogicalReplicationCommand::Start { requested_lsn, .. } = command else {
            panic!("expected START_REPLICATION")
        };
        assert_eq!(requested_lsn, 0x1_0000_0002);
        let command = parse_logical_replication_command(
            "START_REPLICATION SLOT changes LOGICAL 0/0 (proto_version '1', publication_names 'changes_pub,other_pub')",
        )
        .unwrap();
        let LogicalReplicationCommand::Start { publication, .. } = command else {
            panic!("expected START_REPLICATION")
        };
        assert_eq!(publication, "changes_pub,other_pub");
        assert!(
            parse_logical_replication_command(
                "START_REPLICATION SLOT changes LOGICAL 0/0 (proto_version '1')"
            )
            .is_err()
        );
        for input in [
            "START_REPLICATION SLOT changes LOGICAL 0/0 (proto_version '3', publication_names 'changes_pub')",
            "START_REPLICATION SLOT changes LOGICAL 0/0 (proto_version '1', publication_names 'changes_pub', streaming 'true')",
            "START_REPLICATION SLOT changes LOGICAL 0/0 (proto_version '1', proto_version '1', publication_names 'changes_pub')",
            "START_REPLICATION SLOT changes LOGICAL 0/not-lsn (proto_version '1', publication_names 'changes_pub')",
            "START_REPLICATION SLOT changes LOGICAL 0/0 (proto_version '1', publication_names 'changes_pub,')",
            "START_REPLICATION SLOT changes LOGICAL 0/0 (proto_version '1' publication_names 'changes_pub')",
        ] {
            assert!(parse_logical_replication_command(input).is_err(), "{input}");
        }
    }

    #[test]
    fn pgoutput_publication_names_follow_standard_identifier_quoting() {
        let mut budget = Budget::new(4 * core::mem::size_of::<SqlName>());
        let mut publications = FixedVec::new(&mut budget, "test publications", 4).unwrap();
        parse_publication_names("\"sales, west\", Plain, \"say\"\"hi\"", &mut publications)
            .unwrap();
        assert_eq!(publications.as_slice()[0].as_str(), "sales, west");
        assert_eq!(publications.as_slice()[1].as_str(), "plain");
        assert_eq!(publications.as_slice()[2].as_str(), "say\"hi");

        for input in ["\"unterminated", "name trailing", "name,,other", ""] {
            publications.clear();
            assert!(
                parse_publication_names(input, &mut publications).is_err(),
                "{input}"
            );
        }
    }

    #[test]
    fn identify_system_command_accepts_simple_query_terminators() {
        assert!(is_identify_system("IDENTIFY_SYSTEM"));
        assert!(is_identify_system(" identify_system ; \n"));
        assert!(!is_identify_system("IDENTIFY_SYSTEM; SELECT 1"));
    }

    #[test]
    fn logical_replication_keeps_simple_sql_out_of_replication_command_dispatch() {
        assert!(is_replication_command(
            ReplicationMode::Logical,
            "IDENTIFY_SYSTEM"
        ));
        assert!(is_replication_command(
            ReplicationMode::Logical,
            "CREATE_REPLICATION_SLOT changes LOGICAL pgoutput"
        ));
        assert!(!is_replication_command(
            ReplicationMode::Logical,
            "SELECT 1"
        ));
        assert!(is_replication_command(
            ReplicationMode::Physical,
            "SELECT 1"
        ));
        assert!(!is_replication_command(
            ReplicationMode::None,
            "IDENTIFY_SYSTEM"
        ));
    }

    #[test]
    fn standby_status_requires_an_ordered_complete_frontier() {
        let mut status = [0_u8; 34];
        status[0] = b'r';
        status[1..9].copy_from_slice(&30_u64.to_be_bytes());
        status[9..17].copy_from_slice(&20_u64.to_be_bytes());
        status[17..25].copy_from_slice(&10_u64.to_be_bytes());
        status[25..33].copy_from_slice(&42_i64.to_be_bytes());
        status[33] = 1;
        assert_eq!(
            standby_status_update(&status),
            Some(StandbyStatusUpdate {
                flush_lsn: 20,
                reply_requested: true,
            })
        );

        status[17..25].copy_from_slice(&21_u64.to_be_bytes());
        assert_eq!(standby_status_update(&status), None);
        status[17..25].copy_from_slice(&10_u64.to_be_bytes());
        status[33] = 2;
        assert_eq!(standby_status_update(&status), None);
        assert_eq!(standby_status_update(&status[..33]), None);
    }
}
