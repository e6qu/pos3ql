//! Per-connection protocol state machine. Owns fixed receive/send buffers
//! and the per-statement SQL arena; all of them are allocated once at
//! server startup and reused across connections.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;

use crate::config::Config;
use crate::mem::arena::Arena;
use crate::pg::auth::{AuthMode, ScramFlow, ScramServer, ScramStep};
use crate::mem::buffer::FixedBuf;
use crate::mem::budget::{Budget, BudgetError};
use crate::sql::eval::sqlstate;
use crate::sql::parser::Parser;
use crate::sql::guc::GucState;
use crate::sql::prep::SqlPreparedPool;
use crate::sql::txn::TxnState;
use crate::sql::types::Datum;
use crate::sql::Engine;
use crate::storage::SqlName;

use super::respond::{Responder, ResultFmt, MAX_RESULT_COLS};
use super::wire::{self, MsgIn, WireFull};
use super::REPORTED_SERVER_VERSION;

/// Most parameters one Bind may carry.
pub const MAX_BIND_PARAMS: usize = 32;

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
    prepared: Vec<Prepared>,
    portals: Vec<Portal>,
    phase: Phase,
    /// Negotiated protocol minor version (major is always 3).
    minor: u16,
    id: i32,
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
            prepared,
            portals,
            phase: Phase::Startup,
            minor: 0,
            id: 0,
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
        for p in &mut self.prepared {
            p.active = false;
        }
        for p in &mut self.portals {
            p.active = false;
        }
        self.phase = Phase::Startup;
        self.minor = 0;
        self.id = id;
    }

    pub fn close(&mut self) -> Option<TcpStream> {
        self.stream.take()
    }

    pub fn is_open(&self) -> bool {
        self.stream.is_some()
    }

    pub fn stream(&self) -> &TcpStream {
        self.stream.as_ref().expect("connection is open")
    }

    pub fn wants_write(&self) -> bool {
        !self.send.is_empty()
    }

    pub fn on_readable(
        &mut self,
        engine: &mut Engine,
        cancel_key: &[u8],
        auth: &AuthContext,
    ) -> After {
        let Some(stream) = self.stream.as_mut() else {
            return After::Close;
        };
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
        match stream.read(space) {
            Ok(0) => return After::Close,
            Ok(n) => self.recv.advance(n),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return After::Close,
        }
        let after = self.process(engine, cancel_key, auth);
        match self.flush() {
            Ok(()) => after,
            Err(()) => After::Close,
        }
    }

    pub fn on_writable(&mut self) -> After {
        match self.flush() {
            Ok(()) => After::Continue,
            Err(()) => After::Close,
        }
    }

    fn process(&mut self, engine: &mut Engine, cancel_key: &[u8], auth: &AuthContext) -> After {
        loop {
            let after = match self.phase {
                Phase::Startup => self.process_startup(cancel_key, auth),
                Phase::AwaitPassword | Phase::AwaitSaslInit | Phase::AwaitSaslFinal => {
                    self.process_auth(cancel_key, auth)
                }
                Phase::Ready | Phase::SkipToSync => self.process_message(engine),
            };
            match after {
                Step::NeedMoreData => return After::Continue,
                Step::Continue => {}
                Step::Close => return After::Close,
            }
        }
    }

    fn process_startup(&mut self, cancel_key: &[u8], auth: &AuthContext) -> Step {
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
            wire::REQUEST_SSL | wire::REQUEST_GSSENC => {
                self.recv.consume(len);
                // No TLS/GSS support: 'N' tells the client to continue in
                // the clear, per the protocol flow.
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
                let result = self.handle_startup_packet(len, version, cancel_key, auth);
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
                "database" | "options" | "replication" => {}
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
                        && let Err(e) = self.guc.set(key, value) {
                            guc_error = Some(e);
                        }
                }
            }
        }
        if !user_seen {
            let mut responder = Responder::new(&mut self.send);
            let _ = responder.error("28000", "no PostgreSQL user name specified in startup packet");
            return Step::Close;
        }
        if let Some(e) = guc_error {
            let mut responder = Responder::new(&mut self.send);
            let _ = responder.error(e.sqlstate, e.message.as_str());
            return Step::Close;
        }

        self.minor = requested_minor.min(wire::NEWEST_MINOR as u16);

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
            AuthMode::Trust => self.finish_startup(cancel_key),
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
    fn finish_startup(&mut self, cancel_key: &[u8]) -> Step {
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
            let key = if minor >= 2 { cancel_key } else { &cancel_key[..4] };
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
    fn process_auth(&mut self, cancel_key: &[u8], auth: &AuthContext) -> Step {
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
                let ok = pass.len() == auth.password.len()
                    && pass
                        .bytes()
                        .zip(auth.password.bytes())
                        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                        == 0;
                if ok {
                    self.finish_startup(cancel_key)
                } else {
                    auth_failed(&mut self.send)
                }
            }
            Phase::AwaitSaslInit => {
                let Some(server) = &auth.scram else {
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
                let rc = unsafe {
                    libc::getentropy(nonce.as_mut_ptr().cast(), nonce.len())
                };
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
                let Some(server) = &auth.scram else {
                    return Step::Close;
                };
                let Ok(client_final) = core::str::from_utf8(payload) else {
                    return Step::Close;
                };
                match self.scram.finish(server, client_final) {
                    Ok(ScramStep::Final(sig)) => {
                        {
                            let mut responder = Responder::new(&mut self.send);
                            if responder.auth_sasl_final(sig.as_str()).is_err() {
                                return Step::Close;
                            }
                        }
                        self.finish_startup(cancel_key)
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
                let _ = responder.error(sqlstate::PROTOCOL_VIOLATION, "unknown frontend message type");
                Step::Close
            }
        };
        if !matches!(step, Step::Close) && msg_type != wire::FMSG_QUERY {
            self.recv.consume(total);
        }
        step
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
            return ext_err(&mut self.send, &mut self.phase, sqlstate::PROTOCOL_VIOLATION, "malformed Parse message");
        };

        // Validate now so Parse errors surface at Parse, like PostgreSQL.
        self.arena.reset();
        let n_params = {
            let mut parser = match Parser::new(query, &self.arena) {
                Ok(p) => p,
                Err(e) => return ext_err(&mut self.send, &mut self.phase, sqlstate::SYNTAX_ERROR, e.message.as_str()),
            };
            match parser.next_stmt() {
                Ok(_first) => {}
                Err(e) => return ext_err(&mut self.send, &mut self.phase, sqlstate::SYNTAX_ERROR, e.message.as_str()),
            }
            match parser.next_stmt() {
                Ok(None) => {}
                _ => {
                    return ext_err(&mut self.send, &mut self.phase, 
                        sqlstate::SYNTAX_ERROR,
                        "cannot insert multiple commands into a prepared statement",
                    )
                }
            }
            parser.max_param()
        };
        if n_params as usize > MAX_BIND_PARAMS {
            return ext_err(&mut self.send, &mut self.phase, 
                crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many parameters (the limit is 32)",
            );
        }

        // Named statements may not be redefined; the unnamed one always is.
        let slot = if name.is_empty() {
            self.prepared.iter().position(|p| p.active && p.name.as_str().is_empty())
                .or_else(|| self.prepared.iter().position(|p| !p.active))
        } else if self
            .prepared
            .iter()
            .any(|p| p.active && p.name.as_str() == name)
        {
            return ext_err(&mut self.send, &mut self.phase, 
                crate::sql::eval::sqlstate::DUPLICATE_PREPARED_STATEMENT,
                "prepared statement already exists",
            );
        } else {
            self.prepared.iter().position(|p| !p.active)
        };
        let Some(slot) = slot else {
            return ext_err(&mut self.send, &mut self.phase, "54000", "too many prepared statements");
        };
        let Ok(sql_name) = SqlName::parse(name) else {
            return ext_err(&mut self.send, &mut self.phase, "42622", "statement name too long");
        };
        let entry = &mut self.prepared[slot];
        entry.text.clear();
        if !entry.text.append(query.as_bytes()) {
            entry.active = false;
            return ext_err(&mut self.send, &mut self.phase, "54000", "statement text exceeds prepared_bytes");
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
            Ok((portal, statement, n_params, spans, formats, values, result_formats))
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
                    )
                }
                Err(BindProblem::TooManyResultCols) => {
                    return ext_err(
                        &mut self.send,
                        &mut self.phase,
                        crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "too many result columns requested in binary format",
                    )
                }
                Err(BindProblem::TooManyParams) => {
                    return ext_err(
                        &mut self.send,
                        &mut self.phase,
                        crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "too many parameters (the limit is 32)",
                    )
                }
            };

        let Some(stmt_slot) = self
            .prepared
            .iter()
            .position(|p| p.active && p.name.as_str() == stmt_name)
        else {
            return ext_err(&mut self.send, &mut self.phase, "26000", "prepared statement does not exist");
        };
        if n_params != self.prepared[stmt_slot].n_params as usize {
            return ext_err(&mut self.send, &mut self.phase, 
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
                return ext_err(&mut self.send, &mut self.phase, "22021", "invalid UTF-8 in parameter value");
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
            return ext_err(&mut self.send, &mut self.phase, "42622", "portal name too long");
        };
        // Copy the raw parameter area; spans index into it.
        let portal = &mut self.portals[slot];
        portal.params.clear();
        if !portal.params.append(values) {
            return ext_err(&mut self.send, &mut self.phase, "54000", "parameters exceed portal_bytes");
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
            return ext_err(&mut self.send, &mut self.phase, sqlstate::PROTOCOL_VIOLATION, "malformed Describe message");
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
                return ext_err(&mut self.send, &mut self.phase, 
                    sqlstate::PROTOCOL_VIOLATION,
                    "Describe expects 'S' or 'P'",
                )
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
            && responder.parameter_description(&param_oids[..n_params as usize]).is_err()
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
            return ext_err(&mut self.send, &mut self.phase, sqlstate::PROTOCOL_VIOLATION, "malformed Execute message");
        };
        let Some(portal_slot) = self
            .portals
            .iter()
            .position(|p| p.active && p.name.as_str() == name)
        else {
            return ext_err(&mut self.send, &mut self.phase, "34000", "portal does not exist");
        };

        // A portal already producing rows (executed==true) always drains
        // from its buffer, even for a following Execute with max_rows=0.
        // A fresh portal buffers when paged (max_rows>0), else streams.
        let already_started = self.portals[portal_slot].executed;
        let paged = max_rows > 0 || already_started;
        let need_run = !already_started;

        if need_run {
            self.arena.reset();
            let portal = &mut self.portals[portal_slot];
            let prepared = &self.prepared[portal.statement];
            if !prepared.active {
                return ext_err(&mut self.send, &mut self.phase, "26000", "prepared statement no longer exists");
            }
            let text =
                core::str::from_utf8(prepared.text.readable()).expect("stored from valid UTF-8");
            let mut params = [Datum::Null; MAX_BIND_PARAMS];
            let raw = portal.params.readable();
            for (i, &(offset, len)) in portal.spans.iter().take(portal.n_params as usize).enumerate() {
                if len == u32::MAX {
                    params[i] = Datum::Null;
                    continue;
                }
                let bytes = &raw[offset as usize..(offset + len) as usize];
                if portal.binary[i] {
                    match decode_binary_param(prepared.param_oids[i], bytes, &self.arena) {
                        Ok(v) => params[i] = v,
                        Err(message) => {
                            return ext_err(&mut self.send, &mut self.phase, sqlstate::FEATURE_NOT_SUPPORTED, message)
                        }
                    }
                } else {
                    params[i] = Datum::Text(unsafe { core::str::from_utf8_unchecked(bytes) });
                }
            }

            // Paged execution goes through the portal's result buffer so
            // later Execute messages can continue draining it.
            let rfmt = portal.result_formats;
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
                )
            };
            engine.maybe_checkpoint();
            self.arena.reset();
            match result {
                Ok(true) => {}
                Ok(false) => {
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
                Err(WireFull) => return Step::Close,
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
            return ext_err(&mut self.send, &mut self.phase, sqlstate::PROTOCOL_VIOLATION, "malformed Close message");
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
                return ext_err(&mut self.send, &mut self.phase, 
                    sqlstate::PROTOCOL_VIOLATION,
                    "Close expects 'S' or 'P'",
                )
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
            let mut responder = Responder::new(&mut self.send);
            if let Some(fd) = fd {
                responder = responder.with_flush(fd);
            }
            engine.execute_simple(text, &self.arena, &mut self.txn, &mut self.sqlprep, &mut self.cursors, &mut self.guc, &mut responder)
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
            Ok(()) => {
                let mut responder = Responder::new(&mut self.send);
                match responder.ready_for_query(status) {
                    Ok(()) => Step::Continue,
                    Err(WireFull) => Step::Close,
                }
            }
            Err(WireFull) => {
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

    /// Writes as much of the send buffer as the socket accepts.
    /// `Err` means the connection is broken.
    fn flush(&mut self) -> Result<(), ()> {
        let Some(stream) = self.stream.as_mut() else {
            return Err(());
        };
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

enum Step {
    Continue,
    NeedMoreData,
    Close,
}

fn resp_portal_suspended(responder: &mut Responder) -> Result<(), crate::pg::wire::WireFull> {
    use crate::pg::wire::{MsgOut, MSG_PORTAL_SUSPENDED};
    MsgOut::begin(responder.buffer, MSG_PORTAL_SUSPENDED).finish()
}

/// Decodes a binary-format parameter using its declared type OID
/// (network byte order per the protocol's binary representations). `arena`
/// backs the values (e.g. NUMERIC) that need it.
fn decode_binary_param<'a>(
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
            Ok(Datum::Float8(f64::from(f32::from_be_bytes(b))))
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
            Ok(Datum::Interval(crate::sql::types::Interval { months, days, micros }))
        }
        oids::JSON => core::str::from_utf8(bytes)
            .map(|t| Datum::Json { text: t, jsonb: false })
            .map_err(|_| "invalid UTF-8 in binary json parameter"),
        oids::JSONB => {
            // jsonb send format: a 1-byte version (0x01) then the JSON text.
            let (&ver, rest) = bytes.split_first().ok_or(wrong)?;
            if ver != 1 {
                return Err("unsupported jsonb binary version");
            }
            core::str::from_utf8(rest)
                .map(|t| Datum::Json { text: t, jsonb: true })
                .map_err(|_| "invalid UTF-8 in binary jsonb parameter")
        }
        oids::NUMERIC => {
            let mut buffer = crate::util::StackStr::<96>::new();
            binary_numeric_to_str(bytes, &mut buffer)?;
            crate::sql::numeric::Numeric::parse(buffer.as_str(), arena)
                .map(Datum::Numeric)
                .map_err(|_| "binary numeric out of range")
        }
        _ => Err("binary format for this parameter type is not implemented (use text)"),
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
        if i >= 0 && (i as usize) < ndigits { rd(8 + i as usize * 2) } else { 0 }
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

fn finish_numeric(out: &crate::util::StackStr<96>, wrong: &'static str) -> Result<(), &'static str> {
    if out.is_truncated() { Err(wrong) } else { Ok(()) }
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
        assert_eq!(
            num_str(&[0, 2, 0, 0, 0, 0, 0, 2, 0, 2, 0x13, 0x88]),
            "2.50"
        );
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
}
