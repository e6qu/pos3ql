//! Typed PostgreSQL v3 client framing for bounded outbound connections.
//!
//! It owns one non-blocking socket, but constructs only complete frontend
//! states and parses only complete backend states.  Fixed buffers and typed
//! states keep an incomplete network read from becoming an apply action or
//! acknowledgement.

use std::net::{IpAddr, SocketAddr, TcpStream};
use std::os::fd::FromRawFd;
use std::sync::Arc;

use crate::mem::budget::{Budget, BudgetError};
use crate::mem::buffer::FixedBuf;
use crate::mem::fixed_vec::FixedVec;
use crate::object_store::tls::{ClientTlsConfig, Transport};
use crate::pg::pginput::{self, CopyData};
use crate::pg::pgoutput::ProtocolVersion;
use crate::pg::wire::{self, MsgOut, WireFull};
use crate::storage::SqlName;
use crate::util::StackStr;
use crate::{
    crypto::hmac::hmac_sha256,
    crypto::sha256::sha256,
    pg::auth::{b64_decode, b64_encode},
};

/// The outbound worker currently uses a direct TCP socket.  TLS is a distinct
/// typed transport state; it is never silently inferred from a connection
/// string or replaced with plaintext.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SslMode {
    Disable,
    Prefer,
    Require,
}

/// A fully parsed, bounded publisher endpoint.  Creating this value is the
/// only accepted boundary between SQL's connection literal and the wire
/// client, so omitted endpoint fields cannot acquire process/environment
/// defaults later.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectionInfo {
    host: StackStr<45>,
    port: u16,
    user: StackStr<63>,
    database: StackStr<63>,
    password: Option<StackStr<256>>,
    application_name: Option<StackStr<64>>,
    ssl_mode: SslMode,
}

impl ConnectionInfo {
    pub fn parse(input: &str) -> Result<Self, ConnectionInfoError> {
        let mut parser = ConnectionParser { input, at: 0 };
        let mut host = None;
        let mut port = None;
        let mut user = None;
        let mut database = None;
        let mut password = None;
        let mut application_name = None;
        let mut ssl_mode = None;
        while let Some((key, value)) = parser.pair()? {
            match key {
                "host" if host.replace(value).is_none() => {}
                "port" if port.replace(value).is_none() => {}
                "user" if user.replace(value).is_none() => {}
                "dbname" if database.replace(value).is_none() => {}
                "password" if password.replace(value).is_none() => {}
                "application_name" if application_name.replace(value).is_none() => {}
                "sslmode" if ssl_mode.replace(value).is_none() => {}
                "host" | "port" | "user" | "dbname" | "password" | "application_name"
                | "sslmode" => return Err(ConnectionInfoError::Duplicate),
                _ => return Err(ConnectionInfoError::UnsupportedOption),
            }
        }
        let host = bounded(host.ok_or(ConnectionInfoError::Missing("host"))?)?;
        if host.as_str().parse::<std::net::IpAddr>().is_err() {
            return Err(ConnectionInfoError::NonNumericHost);
        }
        let port = port
            .ok_or(ConnectionInfoError::Missing("port"))?
            .parse::<u16>()
            .map_err(|_| ConnectionInfoError::InvalidPort)?;
        if port == 0 {
            return Err(ConnectionInfoError::InvalidPort);
        }
        let user = bounded(user.ok_or(ConnectionInfoError::Missing("user"))?)?;
        let database = bounded(database.ok_or(ConnectionInfoError::Missing("dbname"))?)?;
        let password = password.map(bounded).transpose()?;
        let application_name = application_name.map(bounded).transpose()?;
        let ssl_mode = match ssl_mode.ok_or(ConnectionInfoError::Missing("sslmode"))? {
            "disable" => SslMode::Disable,
            "prefer" => SslMode::Prefer,
            "require" => SslMode::Require,
            _ => return Err(ConnectionInfoError::UnsupportedSslMode),
        };
        Ok(Self {
            host,
            port,
            user,
            database,
            password,
            application_name,
            ssl_mode,
        })
    }

    pub fn host(&self) -> &str {
        self.host.as_str()
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn user(&self) -> &str {
        self.user.as_str()
    }

    pub fn database(&self) -> &str {
        self.database.as_str()
    }

    pub fn password(&self) -> Option<&str> {
        self.password.as_ref().map(StackStr::as_str)
    }

    pub fn application_name(&self) -> Option<&str> {
        self.application_name.as_ref().map(StackStr::as_str)
    }

    pub(crate) fn for_subscription(mut self, subscription: SqlName) -> Self {
        if self.application_name.is_none() {
            self.application_name = Some(StackStr::from_str(subscription.as_str()));
        }
        self
    }

    pub fn ssl_mode(&self) -> SslMode {
        self.ssl_mode
    }

    pub(crate) fn for_foreign(
        host: &str,
        port: u16,
        user: &str,
        database: &str,
        password: Option<&str>,
        application_name: &str,
        ssl_mode: SslMode,
    ) -> Result<Self, ConnectionInfoError> {
        if host.parse::<IpAddr>().is_err() {
            return Err(ConnectionInfoError::NonNumericHost);
        }
        if port == 0 {
            return Err(ConnectionInfoError::InvalidPort);
        }
        Ok(Self {
            host: bounded(host)?,
            port,
            user: bounded(user)?,
            database: bounded(database)?,
            password: password.map(bounded).transpose()?,
            application_name: Some(bounded(application_name)?),
            ssl_mode,
        })
    }

    fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(
            self.host
                .as_str()
                .parse::<IpAddr>()
                .expect("ConnectionInfo validates a numeric host"),
            self.port,
        )
    }
}

/// Checks the PostgreSQL key/value conninfo grammar without claiming that the
/// local replication client can use every PostgreSQL connection parameter.
/// Disabled subscriptions retain this validated catalog text without opening a
/// remote connection; enabling one requires `ConnectionInfo` below.
pub(crate) fn validate_connection_syntax(input: &str) -> Result<(), ConnectionInfoError> {
    let mut parser = ConnectionParser { input, at: 0 };
    while parser.pair()?.is_some() {}
    Ok(())
}

fn bounded<const N: usize>(value: &str) -> Result<StackStr<N>, ConnectionInfoError> {
    if value.is_empty() || value.as_bytes().contains(&0) {
        return Err(ConnectionInfoError::InvalidValue);
    }
    let value = StackStr::from_str(value);
    (!value.is_truncated())
        .then_some(value)
        .ok_or(ConnectionInfoError::Limit)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionInfoError {
    Missing(&'static str),
    Duplicate,
    InvalidValue,
    InvalidPort,
    NonNumericHost,
    UnsupportedOption,
    UnsupportedSslMode,
    Limit,
    Syntax,
}

struct ConnectionParser<'a> {
    input: &'a str,
    at: usize,
}

impl<'a> ConnectionParser<'a> {
    fn pair(&mut self) -> Result<Option<(&'a str, &'a str)>, ConnectionInfoError> {
        self.skip_space();
        if self.at == self.input.len() {
            return Ok(None);
        }
        let key_start = self.at;
        while let Some(byte) = self.input.as_bytes().get(self.at) {
            if *byte == b'=' {
                break;
            }
            if byte.is_ascii_whitespace() || *byte == b'\'' || *byte == b'\\' {
                return Err(ConnectionInfoError::Syntax);
            }
            self.at += 1;
        }
        if self.at == key_start || self.input.as_bytes().get(self.at) != Some(&b'=') {
            return Err(ConnectionInfoError::Syntax);
        }
        let key = &self.input[key_start..self.at];
        self.at += 1;
        let value = if self.input.as_bytes().get(self.at) == Some(&b'\'') {
            self.at += 1;
            let start = self.at;
            while let Some(byte) = self.input.as_bytes().get(self.at) {
                if *byte == b'\'' {
                    let value = &self.input[start..self.at];
                    self.at += 1;
                    return Ok(Some((key, value)));
                }
                if *byte == b'\\' {
                    return Err(ConnectionInfoError::Syntax);
                }
                self.at += 1;
            }
            return Err(ConnectionInfoError::Syntax);
        } else {
            let start = self.at;
            while self
                .input
                .as_bytes()
                .get(self.at)
                .is_some_and(|byte| !byte.is_ascii_whitespace())
            {
                self.at += 1;
            }
            if start == self.at {
                return Err(ConnectionInfoError::Syntax);
            }
            &self.input[start..self.at]
        };
        Ok(Some((key, value)))
    }

    fn skip_space(&mut self) {
        while self
            .input
            .as_bytes()
            .get(self.at)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.at += 1;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    Malformed,
    UnsupportedAuthentication,
    Pgoutput(pginput::DecodeError),
}

#[derive(Debug)]
pub enum ClientError {
    Budget(BudgetError),
    Io(std::io::Error),
    Protocol(FrameError),
    WireFull,
    MissingApplicationName,
    UnsupportedTls,
    Authentication,
    Publisher(PublisherDiagnostic),
    PublisherError,
    Closed,
}

impl From<BudgetError> for ClientError {
    fn from(error: BudgetError) -> Self {
        Self::Budget(error)
    }
}

impl From<std::io::Error> for ClientError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<FrameError> for ClientError {
    fn from(error: FrameError) -> Self {
        Self::Protocol(error)
    }
}

impl core::fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Budget(error) => write!(formatter, "{error}"),
            Self::Io(error) => write!(formatter, "PostgreSQL transport: {error}"),
            Self::Protocol(error) => write!(formatter, "PostgreSQL protocol: {error:?}"),
            Self::WireFull => write!(formatter, "PostgreSQL send buffer is full"),
            Self::MissingApplicationName => {
                write!(formatter, "PostgreSQL connection requires application_name")
            }
            Self::UnsupportedTls => {
                write!(formatter, "PostgreSQL TLS transport is not configured")
            }
            Self::Authentication => write!(formatter, "PostgreSQL authentication cannot proceed"),
            Self::Publisher(error) => {
                write!(
                    formatter,
                    "publisher error [{}]: {}",
                    error.sqlstate,
                    error.message.as_str()
                )
            }
            Self::PublisherError => write!(formatter, "publisher rejected replication startup"),
            Self::Closed => write!(formatter, "publisher closed replication transport"),
        }
    }
}

/// A PostgreSQL ErrorResponse reduced to the stable fields needed for
/// diagnostics and typed recovery decisions. Unknown fields remain protocol
/// compatible without entering runtime state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublisherDiagnostic {
    pub sqlstate: crate::sql::eval::SqlState,
    pub message: StackStr<192>,
}

impl std::error::Error for ClientError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClientState {
    Idle,
    Connecting,
    AwaitingSslResponse,
    AwaitingAuthentication,
    AwaitingReady,
    AwaitingSlotAlter,
    AwaitingSlotAlterReady,
    AwaitingSlotResult,
    SnapshotReady,
    AwaitingDropResult,
    CommandComplete,
    SqlReady,
    AwaitingSql,
    CopyOut,
    AwaitingCopyBoth,
    Streaming,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClientPurpose {
    Unbound,
    Stream,
    CreateSlot,
    DropSlot,
    Sql,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(
    clippy::large_enum_variant,
    reason = "SQL rows retain bounded inline columns; indirection would require runtime allocation"
)]
pub enum SqlEvent<'a> {
    RowDescription { fields: u16 },
    DataRow(SqlDataRow<'a>),
    CopyOut { fields: u16, binary: bool },
    CopyData(&'a [u8]),
    CopyDone,
    CommandComplete { tag: &'a str },
    Ready { transaction_status: u8 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SqlDataRow<'a> {
    columns: [Option<&'a [u8]>; crate::storage::MAX_COLUMNS],
    count: usize,
}

impl<'a> SqlDataRow<'a> {
    pub fn columns(&self) -> &[Option<&'a [u8]>] {
        &self.columns[..self.count]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(
    clippy::large_enum_variant,
    reason = "pgoutput and SQL events remain inline so the runtime transport cannot allocate"
)]
pub enum ClientEvent<'a> {
    Replication(CopyData<'a>),
    Sql(SqlEvent<'a>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotSnapshot {
    pub consistent_lsn: u64,
    pub name: StackStr<128>,
}

/// Bounded generic PostgreSQL replication transport.  It owns no apply state:
/// the reactor feeds writable/readable readiness here and sends the returned
/// `CopyData` frames to exactly one `SubscriptionApply` instance.
pub struct ReplicationClient {
    endpoint: Option<ConnectionInfo>,
    slot: SqlName,
    publications: FixedVec<SqlName>,
    start_lsn: u64,
    protocol: ProtocolVersion,
    behavior: crate::storage::SubscriptionBehavior,
    manage_slot_behavior: bool,
    decode: pginput::DecodeState,
    stream: Option<Transport>,
    receive: FixedBuf,
    send: FixedBuf,
    state: ClientState,
    scram: Option<ScramClient>,
    tls: Option<Arc<rustls::ClientConfig>>,
    purpose: ClientPurpose,
    slot_snapshot: Option<SlotSnapshot>,
}

/// All immutable replication-stream parameters are parsed and selected before
/// socket creation.  Passing one value makes a client unable to accidentally
/// combine an endpoint with another subscription's slot or progress frontier.
#[derive(Clone, Copy)]
pub(crate) struct ReplicationClientSetup<'a> {
    pub endpoint: ConnectionInfo,
    pub slot: SqlName,
    pub publications: &'a [SqlName],
    pub start_lsn: u64,
    pub protocol: ProtocolVersion,
    pub behavior: crate::storage::SubscriptionBehavior,
    pub manage_slot_behavior: bool,
}

impl ReplicationClient {
    pub const fn budget_bytes(
        publication_capacity: usize,
        receive_bytes: usize,
        send_bytes: usize,
    ) -> usize {
        publication_capacity * core::mem::size_of::<SqlName>() + receive_bytes + send_bytes
    }

    #[cfg(test)]
    pub(crate) fn new(
        budget: &mut Budget,
        setup: ReplicationClientSetup<'_>,
        receive_bytes: usize,
        send_bytes: usize,
    ) -> Result<Self, ClientError> {
        let mut client = Self::new_unbound(
            budget,
            setup.publications.len(),
            receive_bytes,
            send_bytes,
            None,
        )?;
        client.bind(setup)?;
        Ok(client)
    }

    /// Reserves a worker's complete transport memory at startup.  Binding a
    /// durable subscription later opens a socket but never allocates buffers.
    pub(crate) fn new_unbound(
        budget: &mut Budget,
        publication_capacity: usize,
        receive_bytes: usize,
        send_bytes: usize,
        tls: Option<&ClientTlsConfig>,
    ) -> Result<Self, ClientError> {
        Ok(Self {
            endpoint: None,
            slot: SqlName::EMPTY,
            publications: FixedVec::new(budget, "subscription_publications", publication_capacity)?,
            start_lsn: 0,
            protocol: ProtocolVersion::V4,
            behavior: crate::storage::SubscriptionBehavior::POSTGRESQL_18_DEFAULT,
            manage_slot_behavior: false,
            decode: pginput::DecodeState::new(true),
            stream: None,
            receive: FixedBuf::new(budget, "subscription_receive", receive_bytes)?,
            send: FixedBuf::new(budget, "subscription_send", send_bytes)?,
            state: ClientState::Idle,
            scram: None,
            tls: tls.map(|tls| tls.config.clone()),
            purpose: ClientPurpose::Unbound,
            slot_snapshot: None,
        })
    }

    pub(crate) fn bind(&mut self, setup: ReplicationClientSetup<'_>) -> Result<(), ClientError> {
        if self.state != ClientState::Idle || self.stream.is_some() {
            return Err(ClientError::Protocol(FrameError::Malformed));
        }
        let ReplicationClientSetup {
            endpoint,
            slot,
            publications,
            start_lsn,
            protocol,
            behavior,
            manage_slot_behavior,
        } = setup;
        if endpoint.ssl_mode() != SslMode::Disable && self.tls.is_none() {
            return Err(ClientError::UnsupportedTls);
        }
        if endpoint.application_name().is_none() {
            return Err(ClientError::MissingApplicationName);
        }
        if publications.is_empty() {
            return Err(ClientError::WireFull);
        }
        if publications.len() > self.publications.capacity() {
            return Err(ClientError::WireFull);
        }
        for &publication in publications {
            self.publications
                .push(publication)
                .expect("validated against fixed publication capacity");
        }
        match open_nonblocking(endpoint.socket_addr()) {
            Ok(stream) => {
                self.endpoint = Some(endpoint);
                self.slot = slot;
                self.start_lsn = start_lsn;
                self.protocol = protocol;
                self.behavior = behavior;
                self.manage_slot_behavior = manage_slot_behavior;
                self.decode = pginput::DecodeState::new(matches!(
                    behavior.streaming,
                    crate::storage::SubscriptionStreaming::Parallel
                ));
                self.purpose = ClientPurpose::Stream;
                self.stream = Some(Transport::plain(stream));
                self.state = ClientState::Connecting;
                Ok(())
            }
            Err(error) => {
                self.publications.clear();
                Err(error.into())
            }
        }
    }

    pub fn unbind(&mut self) {
        self.stream = None;
        self.endpoint = None;
        self.slot = SqlName::EMPTY;
        self.publications.clear();
        self.start_lsn = 0;
        self.behavior = crate::storage::SubscriptionBehavior::POSTGRESQL_18_DEFAULT;
        self.manage_slot_behavior = false;
        self.decode = pginput::DecodeState::new(true);
        self.receive.clear();
        self.send.clear();
        self.scram = None;
        self.purpose = ClientPurpose::Unbound;
        self.slot_snapshot = None;
        self.state = ClientState::Idle;
    }

    pub(crate) fn bind_create_slot(
        &mut self,
        endpoint: ConnectionInfo,
        slot: SqlName,
        behavior: crate::storage::SubscriptionBehavior,
    ) -> Result<(), ClientError> {
        self.bind_command(endpoint, slot, ClientPurpose::CreateSlot)?;
        self.behavior = behavior;
        Ok(())
    }

    pub(crate) fn bind_drop_slot(
        &mut self,
        endpoint: ConnectionInfo,
        slot: SqlName,
    ) -> Result<(), ClientError> {
        self.bind_command(endpoint, slot, ClientPurpose::DropSlot)
    }

    pub(crate) fn bind_sql(&mut self, endpoint: ConnectionInfo) -> Result<(), ClientError> {
        self.bind_command(endpoint, SqlName::EMPTY, ClientPurpose::Sql)
    }

    fn bind_command(
        &mut self,
        endpoint: ConnectionInfo,
        slot: SqlName,
        purpose: ClientPurpose,
    ) -> Result<(), ClientError> {
        if self.state != ClientState::Idle || self.stream.is_some() {
            return Err(ClientError::Protocol(FrameError::Malformed));
        }
        if endpoint.ssl_mode() != SslMode::Disable && self.tls.is_none() {
            return Err(ClientError::UnsupportedTls);
        }
        if endpoint.application_name().is_none() {
            return Err(ClientError::MissingApplicationName);
        }
        let stream = open_nonblocking(endpoint.socket_addr())?;
        self.endpoint = Some(endpoint);
        self.slot = slot;
        self.purpose = purpose;
        self.stream = Some(Transport::plain(stream));
        self.state = ClientState::Connecting;
        Ok(())
    }

    pub(crate) fn slot_snapshot(&self) -> Option<SlotSnapshot> {
        (self.state == ClientState::SnapshotReady)
            .then_some(self.slot_snapshot)
            .flatten()
    }

    pub(crate) fn command_complete(&self) -> bool {
        self.state == ClientState::CommandComplete
    }

    pub(crate) fn query(&mut self, query: &str) -> Result<(), ClientError> {
        if self.state != ClientState::SqlReady || query.is_empty() || query.as_bytes().contains(&0)
        {
            return Err(ClientError::Protocol(FrameError::Malformed));
        }
        let mark = self.send.mark();
        let mut message = MsgOut::begin(&mut self.send, wire::FMSG_QUERY);
        message.bytes(query.as_bytes());
        message.u8(0);
        if message.finish().is_err() {
            self.send.truncate_to(mark);
            return Err(ClientError::WireFull);
        }
        self.state = ClientState::AwaitingSql;
        Ok(())
    }

    pub fn raw_fd(&self) -> std::os::fd::RawFd {
        self.stream
            .as_ref()
            .expect("bound replication worker")
            .raw_fd()
    }

    pub fn wants_write(&self) -> bool {
        !self.send.is_empty()
            || self.state == ClientState::Connecting
            || self.stream.as_ref().is_some_and(Transport::wants_write)
    }

    pub fn is_streaming(&self) -> bool {
        self.state == ClientState::Streaming
    }

    fn queue_startup(&mut self) -> Result<(), ClientError> {
        let endpoint = self.endpoint.expect("bound replication worker");
        startup_packet(
            &mut self.send,
            endpoint.user(),
            endpoint.database(),
            endpoint
                .application_name()
                .expect("constructor requires application_name"),
            self.purpose != ClientPurpose::Sql,
        )
        .map_err(|_| ClientError::WireFull)?;
        self.state = ClientState::AwaitingAuthentication;
        Ok(())
    }

    /// Advances an established non-blocking transport after write readiness.
    pub fn writable(&mut self) -> Result<(), ClientError> {
        if self.state == ClientState::Connecting {
            let mut error = 0_i32;
            let mut length = core::mem::size_of::<i32>() as libc::socklen_t;
            if unsafe {
                libc::getsockopt(
                    self.raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    (&mut error as *mut i32).cast(),
                    &mut length,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            if error != 0 {
                return Err(std::io::Error::from_raw_os_error(error).into());
            }
            let mut peer = unsafe { core::mem::zeroed::<libc::sockaddr_storage>() };
            let mut peer_len = core::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            if unsafe {
                libc::getpeername(
                    self.raw_fd(),
                    (&mut peer as *mut libc::sockaddr_storage).cast(),
                    &mut peer_len,
                )
            } != 0
            {
                let connection = std::io::Error::last_os_error();
                if matches!(
                    connection.raw_os_error(),
                    Some(libc::ENOTCONN) | Some(libc::EINPROGRESS)
                ) {
                    return Ok(());
                }
                return Err(connection.into());
            }
            if self.endpoint.expect("bound replication worker").ssl_mode() != SslMode::Disable {
                if !self.send.append(&8_i32.to_be_bytes())
                    || !self.send.append(&80_877_103_i32.to_be_bytes())
                {
                    return Err(ClientError::WireFull);
                }
                self.state = ClientState::AwaitingSslResponse;
            } else {
                self.queue_startup()?;
            }
        }
        while !self.send.is_empty() {
            match self
                .stream
                .as_mut()
                .expect("bound replication worker")
                .queue_nonblocking(self.send.readable())
            {
                Ok(0) => return Err(ClientError::Closed),
                Ok(written) => self.send.consume(written),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }
        self.stream
            .as_mut()
            .expect("bound replication worker")
            .flush_nonblocking()
            .map_err(ClientError::Io)?;
        Ok(())
    }

    /// Reads every complete backend frame currently buffered.  `visit` sees a
    /// CopyData borrow only until it returns, before the receive buffer can be
    /// consumed or compacted.
    pub fn readable(
        &mut self,
        mut visit: impl FnMut(ClientEvent<'_>) -> Result<(), ClientError>,
    ) -> Result<(), ClientError> {
        loop {
            let writable = self.receive.writable();
            if writable.is_empty() {
                return Err(ClientError::WireFull);
            }
            match self
                .stream
                .as_mut()
                .expect("bound replication worker")
                .read_nonblocking(writable)
            {
                Ok(0) => return Err(ClientError::Closed),
                Ok(read) => self.receive.advance(read),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }
        if self.state == ClientState::AwaitingSslResponse {
            let Some(&response) = self.receive.readable().first() else {
                return Ok(());
            };
            self.receive.consume(1);
            if response == b'N'
                && self.endpoint.expect("bound replication worker").ssl_mode() == SslMode::Prefer
            {
                self.queue_startup()?;
                return Ok(());
            }
            if response != b'S' {
                return Err(ClientError::UnsupportedTls);
            }
            let stream = self
                .stream
                .take()
                .expect("bound replication worker")
                .into_plain();
            let endpoint = self.endpoint.expect("bound replication worker");
            let server_name = rustls::pki_types::ServerName::from(
                endpoint
                    .host()
                    .parse::<IpAddr>()
                    .expect("ConnectionInfo validates a numeric host"),
            );
            self.stream = Some(
                Transport::tls(
                    stream,
                    self.tls.as_ref().expect("require TLS has startup config"),
                    &server_name,
                )
                .map_err(ClientError::Io)?,
            );
            self.queue_startup()?;
        }
        loop {
            let parsed = next_frame(self.receive.readable())?;
            let Some((used, frame)) = parsed else {
                break;
            };
            let ReplicationClient {
                endpoint,
                slot,
                publications,
                start_lsn,
                protocol,
                behavior,
                manage_slot_behavior,
                decode,
                purpose,
                slot_snapshot,
                send,
                state,
                scram,
                ..
            } = self;
            let copied = consume_frame(
                FrameSink {
                    send,
                    state,
                    endpoint: endpoint.expect("bound replication worker"),
                    slot: *slot,
                    publications,
                    start_lsn: *start_lsn,
                    protocol: *protocol,
                    behavior: *behavior,
                    manage_slot_behavior: *manage_slot_behavior,
                    decode,
                    purpose: *purpose,
                    slot_snapshot,
                    scram,
                },
                frame,
            )?;
            if let Some(frame) = copied {
                visit(frame)?;
            }
            self.receive.consume(used);
        }
        Ok(())
    }

    pub fn acknowledge(
        &mut self,
        flushed_lsn: u64,
        reply_requested: bool,
    ) -> Result<(), ClientError> {
        if self.state != ClientState::Streaming {
            return Err(ClientError::Protocol(FrameError::Malformed));
        }
        standby_status(&mut self.send, flushed_lsn, reply_requested)
            .map_err(|_| ClientError::WireFull)
    }
}

fn queue_password(
    send: &mut FixedBuf,
    endpoint: ConnectionInfo,
    authentication: Authentication<'_>,
) -> Result<(), ClientError> {
    let password = endpoint.password().ok_or(ClientError::Authentication)?;
    match authentication {
        Authentication::CleartextPassword => {
            let mut message = MsgOut::begin(send, wire::FMSG_PASSWORD);
            message.bytes(password.as_bytes());
            message.u8(0);
            message.finish().map_err(|_| ClientError::WireFull)?;
        }
        Authentication::Md5 { salt } => {
            let mut first = [0_u8; 32];
            let mut first_source = [0_u8; 319];
            let first_len = password.len() + endpoint.user().len();
            if first_len > first_source.len() {
                return Err(ClientError::Authentication);
            }
            first_source[..password.len()].copy_from_slice(password.as_bytes());
            first_source[password.len()..first_len].copy_from_slice(endpoint.user().as_bytes());
            crate::sql::md5::hex(
                &crate::sql::md5::digest(&first_source[..first_len]),
                &mut first,
            );
            let mut second_source = [0_u8; 36];
            second_source[..32].copy_from_slice(&first);
            second_source[32..].copy_from_slice(&salt);
            let mut second = [0_u8; 32];
            crate::sql::md5::hex(&crate::sql::md5::digest(&second_source), &mut second);
            let mut message = MsgOut::begin(send, wire::FMSG_PASSWORD);
            message.bytes(b"md5");
            message.bytes(&second);
            message.u8(0);
            message.finish().map_err(|_| ClientError::WireFull)?;
        }
        _ => return Err(ClientError::Authentication),
    }
    Ok(())
}

struct FrameSink<'a> {
    send: &'a mut FixedBuf,
    state: &'a mut ClientState,
    endpoint: ConnectionInfo,
    slot: SqlName,
    publications: &'a [SqlName],
    start_lsn: u64,
    protocol: ProtocolVersion,
    behavior: crate::storage::SubscriptionBehavior,
    manage_slot_behavior: bool,
    decode: &'a mut pginput::DecodeState,
    purpose: ClientPurpose,
    slot_snapshot: &'a mut Option<SlotSnapshot>,
    scram: &'a mut Option<ScramClient>,
}

fn consume_frame<'a>(
    sink: FrameSink<'_>,
    frame: BackendFrame<'a>,
) -> Result<Option<ClientEvent<'a>>, ClientError> {
    let FrameSink {
        send,
        state,
        endpoint,
        slot,
        publications,
        start_lsn,
        protocol,
        behavior,
        manage_slot_behavior,
        decode,
        purpose,
        slot_snapshot,
        scram,
    } = sink;
    match frame {
        BackendFrame::Authentication(Authentication::Ok) => *state = ClientState::AwaitingReady,
        BackendFrame::Authentication(Authentication::Sasl { mechanisms }) => {
            let client = ScramClient::begin(endpoint.user(), mechanisms)?;
            queue_sasl_initial(send, client.first.as_str())?;
            *scram = Some(client);
        }
        BackendFrame::Authentication(Authentication::SaslContinue { data }) => {
            let client = scram.as_mut().ok_or(ClientError::Authentication)?;
            let final_message = client.continue_with(
                data,
                endpoint.password().ok_or(ClientError::Authentication)?,
            )?;
            queue_sasl_response(send, final_message.as_str())?;
        }
        BackendFrame::Authentication(Authentication::SaslFinal { data }) => {
            scram
                .as_ref()
                .ok_or(ClientError::Authentication)?
                .verify_final(data)?;
        }
        BackendFrame::Authentication(authentication) => {
            queue_password(send, endpoint, authentication)?
        }
        BackendFrame::ReadyForQuery {
            transaction_status: b'I',
        } if *state == ClientState::AwaitingReady => match purpose {
            ClientPurpose::Stream => {
                if manage_slot_behavior {
                    alter_replication_slot(send, slot, behavior)
                        .map_err(|_| ClientError::WireFull)?;
                    *state = ClientState::AwaitingSlotAlter;
                } else {
                    start_replication(send, slot, start_lsn, publications, protocol, behavior)
                        .map_err(|_| ClientError::WireFull)?;
                    *state = ClientState::AwaitingCopyBoth;
                }
            }
            ClientPurpose::CreateSlot => {
                create_replication_slot(send, slot, behavior).map_err(|_| ClientError::WireFull)?;
                *state = ClientState::AwaitingSlotResult;
            }
            ClientPurpose::DropSlot => {
                drop_replication_slot(send, slot).map_err(|_| ClientError::WireFull)?;
                *state = ClientState::AwaitingDropResult;
            }
            ClientPurpose::Sql => {
                *state = ClientState::SqlReady;
                return Ok(Some(ClientEvent::Sql(SqlEvent::Ready {
                    transaction_status: b'I',
                })));
            }
            ClientPurpose::Unbound => {
                return Err(ClientError::Protocol(FrameError::Malformed));
            }
        },
        BackendFrame::RowDescription { fields: 4 } if *state == ClientState::AwaitingSlotResult => {
        }
        BackendFrame::DataRow(row) if *state == ClientState::AwaitingSlotResult => {
            *slot_snapshot = Some(parse_slot_snapshot(row)?);
        }
        BackendFrame::CommandComplete {
            tag: "CREATE_REPLICATION_SLOT",
        } if *state == ClientState::AwaitingSlotResult && slot_snapshot.is_some() => {
            *state = ClientState::SnapshotReady;
        }
        BackendFrame::CommandComplete {
            tag: "DROP_REPLICATION_SLOT",
        } if *state == ClientState::AwaitingDropResult => {
            *state = ClientState::CommandComplete;
        }
        BackendFrame::CommandComplete {
            tag: "ALTER_REPLICATION_SLOT",
        } if *state == ClientState::AwaitingSlotAlter => {
            *state = ClientState::AwaitingSlotAlterReady;
        }
        BackendFrame::ReadyForQuery {
            transaction_status: b'I',
        } if *state == ClientState::AwaitingSlotAlterReady => {
            start_replication(send, slot, start_lsn, publications, protocol, behavior)
                .map_err(|_| ClientError::WireFull)?;
            *state = ClientState::AwaitingCopyBoth;
        }
        BackendFrame::ReadyForQuery {
            transaction_status: b'I',
        } if matches!(
            *state,
            ClientState::SnapshotReady | ClientState::CommandComplete
        ) => {}
        BackendFrame::RowDescription { fields } if *state == ClientState::AwaitingSql => {
            return Ok(Some(ClientEvent::Sql(SqlEvent::RowDescription { fields })));
        }
        BackendFrame::DataRow(row) if *state == ClientState::AwaitingSql => {
            return Ok(Some(ClientEvent::Sql(SqlEvent::DataRow(row))));
        }
        BackendFrame::CopyOut { fields, binary } if *state == ClientState::AwaitingSql => {
            *state = ClientState::CopyOut;
            return Ok(Some(ClientEvent::Sql(SqlEvent::CopyOut { fields, binary })));
        }
        BackendFrame::CopyData(payload) if *state == ClientState::CopyOut => {
            return Ok(Some(ClientEvent::Sql(SqlEvent::CopyData(payload))));
        }
        BackendFrame::CopyDone if *state == ClientState::CopyOut => {
            *state = ClientState::AwaitingSql;
            return Ok(Some(ClientEvent::Sql(SqlEvent::CopyDone)));
        }
        BackendFrame::CommandComplete { tag } if *state == ClientState::AwaitingSql => {
            return Ok(Some(ClientEvent::Sql(SqlEvent::CommandComplete { tag })));
        }
        BackendFrame::ReadyForQuery { transaction_status }
            if *state == ClientState::AwaitingSql && matches!(transaction_status, b'I' | b'T') =>
        {
            *state = ClientState::SqlReady;
            return Ok(Some(ClientEvent::Sql(SqlEvent::Ready {
                transaction_status,
            })));
        }
        BackendFrame::CopyBoth if *state == ClientState::AwaitingCopyBoth => {
            *state = ClientState::Streaming;
        }
        BackendFrame::CopyData(payload) if *state == ClientState::Streaming => {
            let frame =
                pginput::copy_data_with_state(payload, decode).map_err(FrameError::Pgoutput)?;
            return Ok(Some(ClientEvent::Replication(frame)));
        }
        BackendFrame::ParameterStatus { .. }
        | BackendFrame::BackendKeyData
        | BackendFrame::Notice { .. } => {}
        BackendFrame::Error { fields } => {
            return Err(ClientError::Publisher(publisher_diagnostic(fields)?));
        }
        _ => return Err(ClientError::Protocol(FrameError::Malformed)),
    }
    Ok(None)
}

struct ScramClient {
    first: StackStr<160>,
    nonce: StackStr<512>,
    expected_server_signature: [u8; 32],
}

impl ScramClient {
    fn begin(user: &str, mechanisms: &[u8]) -> Result<Self, ClientError> {
        let mut offered = mechanisms.split(|byte| *byte == 0);
        if !offered.any(|mechanism| mechanism == b"SCRAM-SHA-256") {
            return Err(ClientError::Authentication);
        }
        let mut escaped_user = StackStr::<64>::new();
        use core::fmt::Write as _;
        for character in user.chars() {
            match character {
                ',' => write!(escaped_user, "=2C").map_err(|_| ClientError::Authentication)?,
                '=' => write!(escaped_user, "=3D").map_err(|_| ClientError::Authentication)?,
                '\0' => return Err(ClientError::Authentication),
                _ => {
                    write!(escaped_user, "{character}").map_err(|_| ClientError::Authentication)?
                }
            }
        }
        if escaped_user.is_truncated() {
            return Err(ClientError::Authentication);
        }
        let mut raw_nonce = [0_u8; 18];
        if unsafe { libc::getentropy(raw_nonce.as_mut_ptr().cast(), raw_nonce.len()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut nonce = StackStr::<512>::new();
        b64_encode(&raw_nonce, &mut nonce);
        let mut first = StackStr::new();
        write!(first, "n,,n={},r={}", escaped_user.as_str(), nonce.as_str())
            .map_err(|_| ClientError::Authentication)?;
        if first.is_truncated() {
            return Err(ClientError::Authentication);
        }
        Ok(Self {
            first,
            nonce,
            expected_server_signature: [0; 32],
        })
    }

    fn continue_with(&mut self, data: &[u8], password: &str) -> Result<StackStr<256>, ClientError> {
        let server_first = core::str::from_utf8(data).map_err(|_| ClientError::Authentication)?;
        let mut combined_nonce = None;
        let mut salt_b64 = None;
        let mut iterations = None;
        for attribute in server_first.split(',') {
            let Some((key, value)) = attribute.split_once('=') else {
                return Err(ClientError::Authentication);
            };
            let slot = match key {
                "r" => &mut combined_nonce,
                "s" => &mut salt_b64,
                "i" => &mut iterations,
                _ => return Err(ClientError::Authentication),
            };
            if slot.replace(value).is_some() || value.is_empty() {
                return Err(ClientError::Authentication);
            }
        }
        let nonce = combined_nonce.ok_or(ClientError::Authentication)?;
        if !nonce.starts_with(self.nonce.as_str()) || nonce.len() > 96 {
            return Err(ClientError::Authentication);
        }
        let mut salt = [0_u8; 256];
        let salt_len = b64_decode(salt_b64.ok_or(ClientError::Authentication)?, &mut salt)
            .ok_or(ClientError::Authentication)?;
        if salt_len == 0 {
            return Err(ClientError::Authentication);
        }
        let rounds = iterations
            .ok_or(ClientError::Authentication)?
            .parse::<u32>()
            .map_err(|_| ClientError::Authentication)?;
        if !(4096..=1_000_000).contains(&rounds) {
            return Err(ClientError::Authentication);
        }
        let mut final_without_proof = StackStr::<160>::new();
        use core::fmt::Write as _;
        write!(final_without_proof, "c=biws,r={nonce}").map_err(|_| ClientError::Authentication)?;
        if final_without_proof.is_truncated() {
            return Err(ClientError::Authentication);
        }
        let bare = self
            .first
            .as_str()
            .strip_prefix("n,,")
            .expect("constructed SCRAM first");
        let mut auth_message = StackStr::<512>::new();
        write!(
            auth_message,
            "{bare},{server_first},{}",
            final_without_proof.as_str()
        )
        .map_err(|_| ClientError::Authentication)?;
        if auth_message.is_truncated() {
            return Err(ClientError::Authentication);
        }
        let salted = crate::pg::auth::hi(password.as_bytes(), &salt[..salt_len], rounds);
        let client_key = hmac_sha256(&salted, b"Client Key");
        let stored_key = sha256(&client_key);
        let signature = hmac_sha256(&stored_key, auth_message.as_str().as_bytes());
        let mut proof = [0_u8; 32];
        for (out, (key, signature)) in proof.iter_mut().zip(client_key.iter().zip(signature)) {
            *out = *key ^ signature;
        }
        let server_key = hmac_sha256(&salted, b"Server Key");
        self.expected_server_signature = hmac_sha256(&server_key, auth_message.as_str().as_bytes());
        let mut proof_b64 = StackStr::new();
        b64_encode(&proof, &mut proof_b64);
        let mut final_message = StackStr::new();
        write!(
            final_message,
            "{},p={}",
            final_without_proof.as_str(),
            proof_b64.as_str()
        )
        .map_err(|_| ClientError::Authentication)?;
        (!final_message.is_truncated())
            .then_some(final_message)
            .ok_or(ClientError::Authentication)
    }

    fn verify_final(&self, data: &[u8]) -> Result<(), ClientError> {
        let message = core::str::from_utf8(data).map_err(|_| ClientError::Authentication)?;
        let signature = message
            .strip_prefix("v=")
            .filter(|value| !value.contains(','))
            .ok_or(ClientError::Authentication)?;
        let mut decoded = [0_u8; 32];
        if b64_decode(signature, &mut decoded) != Some(decoded.len()) {
            return Err(ClientError::Authentication);
        }
        let mut diff = 0_u8;
        for (expected, actual) in self.expected_server_signature.iter().zip(decoded) {
            diff |= expected ^ actual;
        }
        (diff == 0).then_some(()).ok_or(ClientError::Authentication)
    }
}

fn queue_sasl_initial(send: &mut FixedBuf, first: &str) -> Result<(), ClientError> {
    let mut message = MsgOut::begin(send, wire::FMSG_PASSWORD);
    message.bytes(b"SCRAM-SHA-256\0");
    message.i32(first.len() as i32);
    message.bytes(first.as_bytes());
    message.finish().map_err(|_| ClientError::WireFull)
}

fn queue_sasl_response(send: &mut FixedBuf, final_message: &str) -> Result<(), ClientError> {
    let mut message = MsgOut::begin(send, wire::FMSG_PASSWORD);
    message.bytes(final_message.as_bytes());
    message.finish().map_err(|_| ClientError::WireFull)
}

fn open_nonblocking(address: SocketAddr) -> std::io::Result<TcpStream> {
    let fd = match address {
        SocketAddr::V4(address) => {
            let fd = make_nonblocking_socket(libc::AF_INET)?;
            let mut raw = unsafe { core::mem::zeroed::<libc::sockaddr_in>() };
            raw.sin_family = libc::AF_INET as libc::sa_family_t;
            raw.sin_port = address.port().to_be();
            raw.sin_addr = libc::in_addr {
                s_addr: u32::from_ne_bytes(address.ip().octets()),
            };
            connect_nonblocking(
                fd,
                (&raw const raw).cast(),
                core::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )?;
            fd
        }
        SocketAddr::V6(address) => {
            let fd = make_nonblocking_socket(libc::AF_INET6)?;
            let mut raw = unsafe { core::mem::zeroed::<libc::sockaddr_in6>() };
            raw.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            raw.sin6_port = address.port().to_be();
            raw.sin6_addr = libc::in6_addr {
                s6_addr: address.ip().octets(),
            };
            raw.sin6_scope_id = address.scope_id();
            connect_nonblocking(
                fd,
                (&raw const raw).cast(),
                core::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
            )?;
            fd
        }
    };
    Ok(unsafe { TcpStream::from_raw_fd(fd) })
}

fn make_nonblocking_socket(domain: libc::c_int) -> std::io::Result<libc::c_int> {
    let fd = unsafe { libc::socket(domain, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) != 0 {
            let error = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(error);
        }
    }
    Ok(fd)
}

fn connect_nonblocking(
    fd: libc::c_int,
    raw: *const libc::sockaddr,
    length: libc::socklen_t,
) -> std::io::Result<()> {
    let result = unsafe { libc::connect(fd, raw, length) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if !matches!(
            error.raw_os_error(),
            Some(libc::EINPROGRESS) | Some(libc::EWOULDBLOCK)
        ) {
            unsafe { libc::close(fd) };
            return Err(error);
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Authentication<'a> {
    Ok,
    CleartextPassword,
    Md5 { salt: [u8; 4] },
    Sasl { mechanisms: &'a [u8] },
    SaslContinue { data: &'a [u8] },
    SaslFinal { data: &'a [u8] },
}

#[allow(
    clippy::large_enum_variant,
    reason = "CopyData retains its decoded fixed message rather than allocating an indirection"
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendFrame<'a> {
    Authentication(Authentication<'a>),
    ParameterStatus { name: &'a str, value: &'a str },
    BackendKeyData,
    ReadyForQuery { transaction_status: u8 },
    CopyBoth,
    CopyOut { fields: u16, binary: bool },
    CopyData(&'a [u8]),
    CopyDone,
    RowDescription { fields: u16 },
    DataRow(SqlDataRow<'a>),
    CommandComplete { tag: &'a str },
    Error { fields: &'a [u8] },
    Notice { fields: &'a [u8] },
}

fn row_description_fields(payload: &[u8]) -> Result<u16, FrameError> {
    let count = u16::from_be_bytes(
        payload
            .get(..2)
            .ok_or(FrameError::Malformed)?
            .try_into()
            .unwrap(),
    );
    let mut at = 2;
    for _ in 0..count {
        let _ = cstr_at(payload, &mut at)?;
        at = at.checked_add(18).ok_or(FrameError::Malformed)?;
        if at > payload.len() {
            return Err(FrameError::Malformed);
        }
    }
    (at == payload.len())
        .then_some(count)
        .ok_or(FrameError::Malformed)
}

fn copy_out_fields(payload: &[u8]) -> Result<(u16, bool), FrameError> {
    let binary = match payload.first() {
        Some(0) => false,
        Some(1) => true,
        _ => return Err(FrameError::Malformed),
    };
    let fields = u16::from_be_bytes(
        payload
            .get(1..3)
            .ok_or(FrameError::Malformed)?
            .try_into()
            .unwrap(),
    );
    let expected = 3usize
        .checked_add(usize::from(fields) * 2)
        .ok_or(FrameError::Malformed)?;
    if payload.len() != expected {
        return Err(FrameError::Malformed);
    }
    for format in payload[3..].as_chunks::<2>().0 {
        let format = i16::from_be_bytes(*format);
        if format != i16::from(binary) {
            return Err(FrameError::Malformed);
        }
    }
    Ok((fields, binary))
}

fn data_row(payload: &[u8]) -> Result<SqlDataRow<'_>, FrameError> {
    let count = usize::from(u16::from_be_bytes(
        payload
            .get(..2)
            .ok_or(FrameError::Malformed)?
            .try_into()
            .unwrap(),
    ));
    if count > crate::storage::MAX_COLUMNS {
        return Err(FrameError::Malformed);
    }
    let mut columns = [None; crate::storage::MAX_COLUMNS];
    let mut at = 2;
    for column in &mut columns[..count] {
        let length = i32::from_be_bytes(
            payload
                .get(at..at + 4)
                .ok_or(FrameError::Malformed)?
                .try_into()
                .unwrap(),
        );
        at += 4;
        if length == -1 {
            continue;
        }
        let length: usize = length.try_into().map_err(|_| FrameError::Malformed)?;
        let value = payload
            .get(at..at.checked_add(length).ok_or(FrameError::Malformed)?)
            .ok_or(FrameError::Malformed)?;
        at += length;
        *column = Some(value);
    }
    (at == payload.len())
        .then_some(SqlDataRow { columns, count })
        .ok_or(FrameError::Malformed)
}

fn parse_slot_snapshot(row: SqlDataRow<'_>) -> Result<SlotSnapshot, ClientError> {
    let [slot_name, consistent_lsn, snapshot_name, output_plugin] = row.columns() else {
        return Err(FrameError::Malformed.into());
    };
    let _slot_name = core::str::from_utf8(slot_name.ok_or(FrameError::Malformed)?)
        .map_err(|_| FrameError::Malformed)?;
    let consistent_lsn = core::str::from_utf8(consistent_lsn.ok_or(FrameError::Malformed)?)
        .map_err(|_| FrameError::Malformed)?;
    let snapshot_name = core::str::from_utf8(snapshot_name.ok_or(FrameError::Malformed)?)
        .map_err(|_| FrameError::Malformed)?;
    if *output_plugin != Some(b"pgoutput".as_slice()) {
        return Err(FrameError::Malformed.into());
    }
    let consistent_lsn = parse_lsn(consistent_lsn).ok_or(FrameError::Malformed)?;
    let name = StackStr::from_str(snapshot_name);
    if name.is_truncated() || name.as_str().is_empty() {
        return Err(FrameError::Malformed.into());
    }
    Ok(SlotSnapshot {
        consistent_lsn,
        name,
    })
}

fn cstr_at<'a>(bytes: &'a [u8], at: &mut usize) -> Result<&'a str, FrameError> {
    let rest = bytes.get(*at..).ok_or(FrameError::Malformed)?;
    let length = rest
        .iter()
        .position(|byte| *byte == 0)
        .ok_or(FrameError::Malformed)?;
    let raw = bytes.get(*at..*at + length).ok_or(FrameError::Malformed)?;
    *at += length + 1;
    core::str::from_utf8(raw).map_err(|_| FrameError::Malformed)
}

fn publisher_diagnostic(fields: &[u8]) -> Result<PublisherDiagnostic, FrameError> {
    let mut at = 0;
    let mut sqlstate = None;
    let mut message = None;
    loop {
        let kind = *fields.get(at).ok_or(FrameError::Malformed)?;
        at += 1;
        if kind == 0 {
            if at != fields.len() {
                return Err(FrameError::Malformed);
            }
            break;
        }
        let value = cstr_at(fields, &mut at)?;
        match kind {
            b'C' => {
                if sqlstate.is_some() {
                    return Err(FrameError::Malformed);
                }
                sqlstate = crate::sql::eval::SqlState::parse(value);
                if sqlstate.is_none() {
                    return Err(FrameError::Malformed);
                }
            }
            b'M' => {
                if message.is_some() {
                    return Err(FrameError::Malformed);
                }
                let value = StackStr::from_str(value);
                if value.is_truncated() || value.as_str().is_empty() {
                    return Err(FrameError::Malformed);
                }
                message = Some(value);
            }
            _ => {}
        }
    }
    Ok(PublisherDiagnostic {
        sqlstate: sqlstate.ok_or(FrameError::Malformed)?,
        message: message.ok_or(FrameError::Malformed)?,
    })
}

fn authentication(payload: &[u8]) -> Result<Authentication<'_>, FrameError> {
    let code = i32::from_be_bytes(
        payload
            .get(..4)
            .ok_or(FrameError::Malformed)?
            .try_into()
            .unwrap(),
    );
    match code {
        wire::AUTH_OK if payload.len() == 4 => Ok(Authentication::Ok),
        wire::AUTH_CLEARTEXT if payload.len() == 4 => Ok(Authentication::CleartextPassword),
        5 if payload.len() == 8 => Ok(Authentication::Md5 {
            salt: payload[4..8].try_into().unwrap(),
        }),
        wire::AUTH_SASL if payload.len() >= 5 && payload.ends_with(&[0]) => {
            Ok(Authentication::Sasl {
                mechanisms: &payload[4..],
            })
        }
        wire::AUTH_SASL_CONTINUE if payload.len() > 4 => Ok(Authentication::SaslContinue {
            data: &payload[4..],
        }),
        wire::AUTH_SASL_FINAL if payload.len() > 4 => Ok(Authentication::SaslFinal {
            data: &payload[4..],
        }),
        _ => Err(FrameError::UnsupportedAuthentication),
    }
}

/// Parses one complete backend frame from the front of `bytes`.
///
/// `Ok(None)` means the fixed receive buffer needs more bytes.  A malformed
/// or unsupported frame is never silently discarded.
pub fn next_frame(bytes: &[u8]) -> Result<Option<(usize, BackendFrame<'_>)>, FrameError> {
    if bytes.len() < 5 {
        return Ok(None);
    }
    let length = i32::from_be_bytes(bytes[1..5].try_into().unwrap());
    if length < 4 {
        return Err(FrameError::Malformed);
    }
    let total = 1usize
        .checked_add(length as usize)
        .ok_or(FrameError::Malformed)?;
    if bytes.len() < total {
        return Ok(None);
    }
    let payload = &bytes[5..total];
    let frame = match bytes[0] {
        wire::MSG_AUTHENTICATION => BackendFrame::Authentication(authentication(payload)?),
        wire::MSG_PARAMETER_STATUS => {
            let mut at = 0;
            let name = cstr_at(payload, &mut at)?;
            let value = cstr_at(payload, &mut at)?;
            if at != payload.len() {
                return Err(FrameError::Malformed);
            }
            BackendFrame::ParameterStatus { name, value }
        }
        wire::MSG_BACKEND_KEY_DATA if matches!(payload.len(), 8 | 20) => {
            BackendFrame::BackendKeyData
        }
        wire::MSG_READY_FOR_QUERY if payload.len() == 1 => BackendFrame::ReadyForQuery {
            transaction_status: payload[0],
        },
        // PostgreSQL's logical-replication CopyBoth response has no columns
        // and uses the protocol's text format marker. pgoutput's binary
        // envelopes live inside subsequent CopyData frames.
        wire::MSG_COPY_BOTH_RESPONSE if payload == [0, 0, 0] => BackendFrame::CopyBoth,
        wire::MSG_COPY_OUT_RESPONSE => {
            let (fields, binary) = copy_out_fields(payload)?;
            BackendFrame::CopyOut { fields, binary }
        }
        wire::FMSG_COPY_DATA => BackendFrame::CopyData(payload),
        wire::FMSG_COPY_DONE if payload.is_empty() => BackendFrame::CopyDone,
        wire::MSG_ROW_DESCRIPTION => BackendFrame::RowDescription {
            fields: row_description_fields(payload)?,
        },
        wire::MSG_DATA_ROW => BackendFrame::DataRow(data_row(payload)?),
        wire::MSG_COMMAND_COMPLETE => {
            let mut at = 0;
            let tag = cstr_at(payload, &mut at)?;
            if at != payload.len() {
                return Err(FrameError::Malformed);
            }
            BackendFrame::CommandComplete { tag }
        }
        wire::MSG_ERROR_RESPONSE => BackendFrame::Error { fields: payload },
        wire::MSG_NOTICE_RESPONSE => BackendFrame::Notice { fields: payload },
        _ => return Err(FrameError::Malformed),
    };
    Ok(Some((total, frame)))
}

/// Writes a replication startup packet. It is intentionally not a normal
/// typed frontend message: StartupMessage has a length but no message byte.
pub fn startup(
    buffer: &mut FixedBuf,
    user: &str,
    database: &str,
    application_name: &str,
) -> Result<(), WireFull> {
    startup_packet(buffer, user, database, application_name, true)
}

fn startup_packet(
    buffer: &mut FixedBuf,
    user: &str,
    database: &str,
    application_name: &str,
    replication: bool,
) -> Result<(), WireFull> {
    if [user, database, application_name]
        .iter()
        .any(|value| value.is_empty() || value.as_bytes().contains(&0))
    {
        return Err(WireFull);
    }
    let mark = buffer.mark();
    let ok = buffer.append(&[0, 0, 0, 0])
        && buffer.append(&wire::PROTOCOL_3_0.to_be_bytes())
        && buffer.append(b"user\0")
        && buffer.append(user.as_bytes())
        && buffer.append(&[0])
        && buffer.append(b"database\0")
        && buffer.append(database.as_bytes())
        && buffer.append(&[0]);
    let ok = ok
        && (!replication || buffer.append(b"replication\0database\0"))
        && buffer.append(b"application_name\0")
        && buffer.append(application_name.as_bytes())
        && buffer.append(&[0, 0]);
    if !ok {
        buffer.truncate_to(mark);
        return Err(WireFull);
    }
    let length = (buffer.mark() - mark) as i32;
    buffer.filled_mut()[mark..mark + 4].copy_from_slice(&length.to_be_bytes());
    Ok(())
}

fn append_identifier(out: &mut MsgOut<'_>, identifier: &str) {
    out.u8(b'\"');
    for byte in identifier.bytes() {
        if byte == b'\"' {
            out.u8(b'\"');
        }
        out.u8(byte);
    }
    out.u8(b'\"');
}

fn parse_lsn(value: &str) -> Option<u64> {
    let (high, low) = value.split_once('/')?;
    if high.is_empty() || low.is_empty() || high.len() > 8 || low.len() > 8 {
        return None;
    }
    Some((u64::from_str_radix(high, 16).ok()? << 32) | u64::from_str_radix(low, 16).ok()?)
}

pub(crate) fn create_replication_slot(
    buffer: &mut FixedBuf,
    slot: SqlName,
    behavior: crate::storage::SubscriptionBehavior,
) -> Result<(), WireFull> {
    let mark = buffer.mark();
    let mut message = MsgOut::begin(buffer, wire::FMSG_QUERY);
    message.bytes(b"CREATE_REPLICATION_SLOT ");
    append_identifier(&mut message, slot.as_str());
    message.bytes(b" LOGICAL pgoutput (SNAPSHOT 'export', TWO_PHASE ");
    message.bytes(if behavior.two_phase {
        &b"true"[..]
    } else {
        &b"false"[..]
    });
    message.bytes(b", FAILOVER ");
    message.bytes(if behavior.failover {
        &b"true"[..]
    } else {
        &b"false"[..]
    });
    message.u8(b')');
    message.u8(0);
    if message.finish().is_err() {
        buffer.truncate_to(mark);
        return Err(WireFull);
    }
    Ok(())
}

fn alter_replication_slot(
    buffer: &mut FixedBuf,
    slot: SqlName,
    behavior: crate::storage::SubscriptionBehavior,
) -> Result<(), WireFull> {
    let mark = buffer.mark();
    let mut message = MsgOut::begin(buffer, wire::FMSG_QUERY);
    message.bytes(b"ALTER_REPLICATION_SLOT ");
    append_identifier(&mut message, slot.as_str());
    message.bytes(b" (TWO_PHASE ");
    message.bytes(if behavior.two_phase {
        &b"true"[..]
    } else {
        &b"false"[..]
    });
    message.bytes(b", FAILOVER ");
    message.bytes(if behavior.failover {
        &b"true"[..]
    } else {
        &b"false"[..]
    });
    message.u8(b')');
    message.u8(0);
    if message.finish().is_err() {
        buffer.truncate_to(mark);
        return Err(WireFull);
    }
    Ok(())
}

pub fn drop_replication_slot(buffer: &mut FixedBuf, slot: SqlName) -> Result<(), WireFull> {
    let mark = buffer.mark();
    let mut message = MsgOut::begin(buffer, wire::FMSG_QUERY);
    message.bytes(b"DROP_REPLICATION_SLOT ");
    append_identifier(&mut message, slot.as_str());
    message.u8(0);
    if message.finish().is_err() {
        buffer.truncate_to(mark);
        return Err(WireFull);
    }
    Ok(())
}

/// Starts a pgoutput stream at a durable LSN. Publication names cross as a
/// SQL literal containing separately quoted identifiers, so punctuation in a
/// legal PostgreSQL name cannot alter the replication command.
pub(crate) fn start_replication(
    buffer: &mut FixedBuf,
    slot: SqlName,
    start_lsn: u64,
    publications: &[SqlName],
    version: ProtocolVersion,
    behavior: crate::storage::SubscriptionBehavior,
) -> Result<(), WireFull> {
    if publications.is_empty() {
        return Err(WireFull);
    }
    let mark = buffer.mark();
    let mut message = MsgOut::begin(buffer, wire::FMSG_QUERY);
    message.bytes(b"START_REPLICATION SLOT ");
    append_identifier(&mut message, slot.as_str());
    message.bytes(b" LOGICAL ");
    let high = start_lsn >> 32;
    let low = start_lsn as u32;
    let lsn = crate::stack_format!(17, "{high:X}/{low:X}");
    message.bytes(lsn.as_str().as_bytes());
    message.bytes(b" (proto_version '");
    let protocol = match version {
        value if value == ProtocolVersion::V1 => "1",
        value if value == ProtocolVersion::V2 => "2",
        value if value == ProtocolVersion::V3 => "3",
        value if value == ProtocolVersion::V4 => "4",
        _ => {
            buffer.truncate_to(mark);
            return Err(WireFull);
        }
    };
    message.bytes(protocol.as_bytes());
    message.bytes(b"', publication_names '");
    for (index, publication) in publications.iter().enumerate() {
        if index != 0 {
            message.bytes(b", ");
        }
        append_identifier(&mut message, publication.as_str());
    }
    message.bytes(b"', binary '");
    message.bytes(if behavior.binary {
        &b"true"[..]
    } else {
        &b"false"[..]
    });
    message.bytes(b"', streaming '");
    message.bytes(match behavior.streaming {
        crate::storage::SubscriptionStreaming::Off => b"off",
        crate::storage::SubscriptionStreaming::On => b"on",
        crate::storage::SubscriptionStreaming::Parallel => b"parallel",
    });
    message.bytes(b"', two_phase '");
    message.bytes(if behavior.two_phase {
        &b"true"[..]
    } else {
        &b"false"[..]
    });
    message.bytes(b"', origin '");
    message.bytes(behavior.origin.as_str().as_bytes());
    message.bytes(b"')");
    message.u8(0);
    if message.finish().is_err() {
        buffer.truncate_to(mark);
        return Err(WireFull);
    }
    Ok(())
}

/// Writes a fully ordered StandbyStatusUpdate after local application and its
/// durable cursor have completed. Times are zero because pos3ql does not
/// manufacture a PostgreSQL wall-clock observation.
pub fn standby_status(
    buffer: &mut FixedBuf,
    flushed_lsn: u64,
    reply_requested: bool,
) -> Result<(), WireFull> {
    let mark = buffer.mark();
    let mut message = MsgOut::begin(buffer, wire::FMSG_COPY_DATA);
    message.u8(b'r');
    message.i64(flushed_lsn as i64);
    message.i64(flushed_lsn as i64);
    message.i64(flushed_lsn as i64);
    message.i64(0);
    message.u8(u8::from(reply_requested));
    if message.finish().is_err() {
        buffer.truncate_to(mark);
        return Err(WireFull);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;
    use crate::mem::guard;

    fn buffer() -> FixedBuf {
        FixedBuf::new(&mut Budget::new(4096), "test", 1024).unwrap()
    }

    #[test]
    fn connection_info_requires_one_explicit_bounded_transport_state() {
        guard::forbid_alloc(|| {
            let connection = ConnectionInfo::parse(
                "host=127.0.0.1 port=5432 user=repl dbname=target password='secret' sslmode=disable",
            )
            .unwrap();
            assert_eq!(connection.host(), "127.0.0.1");
            assert_eq!(connection.port(), 5432);
            assert_eq!(connection.user(), "repl");
            assert_eq!(connection.database(), "target");
            assert_eq!(connection.password(), Some("secret"));
            assert_eq!(connection.application_name(), None);
            assert_eq!(connection.ssl_mode(), SslMode::Disable);
        });
        assert!(matches!(
            ConnectionInfo::parse(
                "host=publisher port=5432 user=repl dbname=target sslmode=disable"
            ),
            Err(ConnectionInfoError::NonNumericHost)
        ));
        assert!(matches!(
            ConnectionInfo::parse("host=127.0.0.1 port=5432 user=repl dbname=target"),
            Err(ConnectionInfoError::Missing("sslmode"))
        ));
        assert!(matches!(
            ConnectionInfo::parse(
                "host=127.0.0.1 host=127.0.0.2 port=5432 user=repl dbname=target sslmode=disable"
            ),
            Err(ConnectionInfoError::Duplicate)
        ));
    }

    #[test]
    fn startup_and_start_replication_are_framed_without_allocation() {
        let mut out = buffer();
        guard::forbid_alloc(|| {
            startup(&mut out, "repl", "postgres", "pos3ql subscription").unwrap();
            assert_eq!(
                i32::from_be_bytes(out.readable()[..4].try_into().unwrap()) as usize,
                out.len()
            );
        });
        out.clear();
        let publications = [
            SqlName::parse("first").unwrap(),
            SqlName::parse("a\"b").unwrap(),
        ];
        start_replication(
            &mut out,
            SqlName::parse("slot").unwrap(),
            0xABCD_EF01,
            &publications,
            ProtocolVersion::V4,
            crate::storage::SubscriptionBehavior::POSTGRESQL_18_DEFAULT,
        )
        .unwrap();
        let command = core::str::from_utf8(&out.readable()[5..out.len() - 1]).unwrap();
        assert_eq!(
            command,
            "START_REPLICATION SLOT \"slot\" LOGICAL 0/ABCDEF01 (proto_version '4', publication_names '\"first\", \"a\"\"b\"', binary 'false', streaming 'parallel', two_phase 'false', origin 'any')"
        );
        out.clear();
        create_replication_slot(
            &mut out,
            SqlName::parse("slot").unwrap(),
            crate::storage::SubscriptionBehavior::POSTGRESQL_18_DEFAULT,
        )
        .unwrap();
        let command = core::str::from_utf8(&out.readable()[5..out.len() - 1]).unwrap();
        assert_eq!(
            command,
            "CREATE_REPLICATION_SLOT \"slot\" LOGICAL pgoutput (SNAPSHOT 'export', TWO_PHASE false, FAILOVER false)"
        );
        out.clear();
        let mut behavior = crate::storage::SubscriptionBehavior::POSTGRESQL_18_DEFAULT;
        behavior.failover = true;
        alter_replication_slot(&mut out, SqlName::parse("slot").unwrap(), behavior).unwrap();
        let command = core::str::from_utf8(&out.readable()[5..out.len() - 1]).unwrap();
        assert_eq!(
            command,
            "ALTER_REPLICATION_SLOT \"slot\" (TWO_PHASE false, FAILOVER true)"
        );
    }

    #[test]
    fn backend_frames_are_complete_or_rejected() {
        assert_eq!(next_frame(b"Z\0\0"), Ok(None));
        assert_eq!(
            next_frame(b"Z\0\0\0\x05I"),
            Ok(Some((
                6,
                BackendFrame::ReadyForQuery {
                    transaction_status: b'I'
                }
            )))
        );
        assert_eq!(next_frame(b"Z\0\0\0\x04"), Err(FrameError::Malformed));
        assert_eq!(
            next_frame(b"R\0\0\0\x08\0\0\0\x03abcd"),
            Ok(Some((
                9,
                BackendFrame::Authentication(Authentication::CleartextPassword)
            )))
        );

        let fields = b"SERROR\0C42704\0Mreplication slot does not exist\0\0";
        let diagnostic = publisher_diagnostic(fields).unwrap();
        assert_eq!(diagnostic.sqlstate, "42704");
        assert_eq!(
            diagnostic.message.as_str(),
            "replication slot does not exist"
        );
        assert!(publisher_diagnostic(b"SERROR\0Mmissing code\0\0").is_err());
        assert!(publisher_diagnostic(b"C42704\0Mfirst\0Msecond\0\0").is_err());
    }

    #[test]
    fn status_acknowledges_one_durable_frontier() {
        let mut out = buffer();
        standby_status(&mut out, 9, true).unwrap();
        let payload = &out.readable()[5..];
        assert_eq!(payload[0], b'r');
        assert_eq!(u64::from_be_bytes(payload[1..9].try_into().unwrap()), 9);
        assert_eq!(u64::from_be_bytes(payload[9..17].try_into().unwrap()), 9);
        assert_eq!(u64::from_be_bytes(payload[17..25].try_into().unwrap()), 9);
        assert_eq!(payload[33], 1);
    }

    #[test]
    fn scram_client_proves_publisher_and_never_accepts_an_unoffered_mechanism() {
        let mut client = ScramClient::begin("repl", b"SCRAM-SHA-256\0").unwrap();
        let server = crate::pg::auth::ScramServer::derive("secret", [7; 16], 4096);
        let mut flow = crate::pg::auth::ScramFlow::new();
        let crate::pg::auth::ScramStep::Continue(server_first) = flow
            .first(&server, client.first.as_str(), &[9; 18])
            .unwrap()
        else {
            panic!("server must continue SCRAM")
        };
        let client_final = client
            .continue_with(server_first.as_str().as_bytes(), "secret")
            .unwrap();
        let crate::pg::auth::ScramStep::Final(server_final) =
            flow.finish(&server, client_final.as_str()).unwrap()
        else {
            panic!("server must finish SCRAM")
        };
        client
            .verify_final(server_final.as_str().as_bytes())
            .unwrap();
        assert!(matches!(
            ScramClient::begin("repl", b"SCRAM-SHA-256-PLUS\0"),
            Err(ClientError::Authentication)
        ));
    }

    #[test]
    fn bounded_client_drives_a_raw_postgres_replication_handshake() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::time::{Duration, Instant};

        fn message(kind: u8, payload: &[u8]) -> [u8; 64] {
            let mut out = [0_u8; 64];
            out[0] = kind;
            out[1..5].copy_from_slice(&((payload.len() + 4) as i32).to_be_bytes());
            out[5..5 + payload.len()].copy_from_slice(payload);
            out
        }
        fn read_message(stream: &mut std::net::TcpStream, out: &mut [u8]) -> usize {
            stream.read_exact(&mut out[..5]).unwrap();
            let length = i32::from_be_bytes(out[1..5].try_into().unwrap()) as usize;
            stream.read_exact(&mut out[5..length + 1]).unwrap();
            length + 1
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let publisher = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut received = [0_u8; 512];
            stream.read_exact(&mut received[..4]).unwrap();
            let startup_len = i32::from_be_bytes(received[..4].try_into().unwrap()) as usize;
            stream.read_exact(&mut received[4..startup_len]).unwrap();
            assert!(
                received[..startup_len]
                    .windows(b"replication\0database\0".len())
                    .any(|window| { window == b"replication\0database\0" })
            );
            let auth = message(
                wire::MSG_AUTHENTICATION,
                &wire::AUTH_CLEARTEXT.to_be_bytes(),
            );
            stream.write_all(&auth[..9]).unwrap();
            let password_len = read_message(&mut stream, &mut received);
            assert_eq!(&received[..5], b"p\0\0\0\x0B");
            assert_eq!(&received[5..password_len], b"secret\0");
            let authentication_ok = message(wire::MSG_AUTHENTICATION, &wire::AUTH_OK.to_be_bytes());
            stream.write_all(&authentication_ok[..9]).unwrap();
            let ready = message(wire::MSG_READY_FOR_QUERY, b"I");
            stream.write_all(&ready[..6]).unwrap();
            let query_len = read_message(&mut stream, &mut received);
            assert_eq!(received[0], wire::FMSG_QUERY);
            assert!(
                core::str::from_utf8(&received[5..query_len - 1])
                    .unwrap()
                    .starts_with("START_REPLICATION SLOT \"slot\" LOGICAL 0/0")
            );
            let copy_both = message(wire::MSG_COPY_BOTH_RESPONSE, &[0, 0, 0]);
            stream.write_all(&copy_both[..8]).unwrap();
            let mut keepalive = [0_u8; 18];
            keepalive[0] = b'k';
            keepalive[1..9].copy_from_slice(&41_u64.to_be_bytes());
            keepalive[17] = 1;
            let copy_data = message(wire::FMSG_COPY_DATA, &keepalive);
            stream.write_all(&copy_data[..23]).unwrap();
            let status_len = read_message(&mut stream, &mut received);
            assert_eq!(received[0], wire::FMSG_COPY_DATA);
            assert_eq!(received[5], b'r');
            assert_eq!(u64::from_be_bytes(received[6..14].try_into().unwrap()), 41);
            assert_eq!(received[status_len - 1], 1);
        });
        let endpoint = ConnectionInfo::parse(&format!(
            "host=127.0.0.1 port={port} user=repl dbname=publisher password=secret application_name=apply sslmode=disable"
        ))
        .unwrap();
        let mut budget = Budget::new(16 * 1024);
        let mut client = ReplicationClient::new(
            &mut budget,
            ReplicationClientSetup {
                endpoint,
                slot: SqlName::parse("slot").unwrap(),
                publications: &[SqlName::parse("changes").unwrap()],
                start_lsn: 0,
                protocol: ProtocolVersion::V4,
                behavior: crate::storage::SubscriptionBehavior::POSTGRESQL_18_DEFAULT,
                manage_slot_behavior: false,
            },
            4096,
            4096,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut keepalive = None;
        while (!client.is_streaming() || keepalive.is_none()) && Instant::now() < deadline {
            client.writable().unwrap();
            client
                .readable(|event| match event {
                    ClientEvent::Replication(CopyData::PrimaryKeepalive {
                        end_lsn,
                        reply_requested,
                    }) => {
                        keepalive = Some((end_lsn, reply_requested));
                        Ok(())
                    }
                    ClientEvent::Replication(CopyData::XLogData { .. }) => {
                        Err(ClientError::Protocol(FrameError::Malformed))
                    }
                    ClientEvent::Sql(_) => Err(ClientError::Protocol(FrameError::Malformed)),
                })
                .unwrap();
            std::thread::yield_now();
        }
        assert!(
            client.is_streaming(),
            "publisher handshake did not reach CopyBoth"
        );
        assert_eq!(keepalive, Some((41, true)));
        client.acknowledge(41, true).unwrap();
        client.writable().unwrap();
        publisher.join().unwrap();
    }

    #[test]
    fn bounded_client_negotiates_required_tls_without_losing_reactor_write_interest() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        fn message(kind: u8, payload: &[u8]) -> [u8; 64] {
            let mut out = [0_u8; 64];
            out[0] = kind;
            out[1..5].copy_from_slice(&((payload.len() + 4) as i32).to_be_bytes());
            out[5..5 + payload.len()].copy_from_slice(payload);
            out
        }
        fn read_message(stream: &mut impl Read, out: &mut [u8]) -> usize {
            stream.read_exact(&mut out[..5]).unwrap();
            let length = i32::from_be_bytes(out[1..5].try_into().unwrap()) as usize;
            stream.read_exact(&mut out[5..length + 1]).unwrap();
            length + 1
        }
        let cert_pem = std::fs::read_to_string("tests/data/tls-test-cert.pem").unwrap();
        let key_pem = std::fs::read_to_string("tests/data/tls-test-key.pem").unwrap();
        let certificates = crate::pem::certificates(&cert_pem)
            .unwrap()
            .into_iter()
            .map(rustls::pki_types::CertificateDer::from)
            .collect();
        let key = crate::pem::blocks(&key_pem)
            .unwrap()
            .into_iter()
            .find(|block| block.label.contains("PRIVATE KEY"))
            .map(|block| rustls::pki_types::PrivateKeyDer::try_from(block.der).unwrap())
            .unwrap();
        let server_config = Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(certificates, key)
                .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let publisher = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut ssl_request = [0_u8; 8];
            stream.read_exact(&mut ssl_request).unwrap();
            assert_eq!(ssl_request, [0, 0, 0, 8, 4, 210, 22, 47]);
            stream.write_all(b"S").unwrap();
            let session = rustls::ServerConnection::new(server_config).unwrap();
            let mut tls = rustls::StreamOwned::new(session, stream);
            let mut received = [0_u8; 512];
            tls.read_exact(&mut received[..4]).unwrap();
            let startup_len = i32::from_be_bytes(received[..4].try_into().unwrap()) as usize;
            tls.read_exact(&mut received[4..startup_len]).unwrap();
            assert!(
                received[..startup_len]
                    .windows(b"replication\0database\0".len())
                    .any(|window| window == b"replication\0database\0")
            );
            let auth = message(
                wire::MSG_AUTHENTICATION,
                &wire::AUTH_CLEARTEXT.to_be_bytes(),
            );
            tls.write_all(&auth[..9]).unwrap();
            let password_len = read_message(&mut tls, &mut received);
            assert_eq!(&received[..5], b"p\0\0\0\x0B");
            assert_eq!(&received[5..password_len], b"secret\0");
            let authentication_ok = message(wire::MSG_AUTHENTICATION, &wire::AUTH_OK.to_be_bytes());
            tls.write_all(&authentication_ok[..9]).unwrap();
            let ready = message(wire::MSG_READY_FOR_QUERY, b"I");
            tls.write_all(&ready[..6]).unwrap();
            let query_len = read_message(&mut tls, &mut received);
            assert_eq!(received[0], wire::FMSG_QUERY);
            assert!(
                core::str::from_utf8(&received[5..query_len - 1])
                    .unwrap()
                    .starts_with("START_REPLICATION SLOT \"slot\" LOGICAL 0/0")
            );
            let copy_both = message(wire::MSG_COPY_BOTH_RESPONSE, &[0, 0, 0]);
            tls.write_all(&copy_both[..8]).unwrap();
            // Keep the socket open until the client observes CopyBoth. A
            // publisher is long-lived in production; closing immediately
            // races the final TLS record against EOF under a busy test
            // scheduler and tests teardown rather than protocol progress.
            let mut closed = [0_u8; 1];
            let _ = tls.read(&mut closed);
        });
        let endpoint = ConnectionInfo::parse(&format!(
            "host=127.0.0.1 port={port} user=repl dbname=publisher password=secret application_name=apply sslmode=require"
        ))
        .unwrap();
        let tls =
            crate::object_store::tls::build_client_config("tests/data/tls-test-cert.pem").unwrap();
        let mut budget = Budget::new(16 * 1024);
        let mut client =
            ReplicationClient::new_unbound(&mut budget, 1, 4096, 4096, Some(&tls)).unwrap();
        client
            .bind(ReplicationClientSetup {
                endpoint,
                slot: SqlName::parse("slot").unwrap(),
                publications: &[SqlName::parse("changes").unwrap()],
                start_lsn: 0,
                protocol: ProtocolVersion::V4,
                behavior: crate::storage::SubscriptionBehavior::POSTGRESQL_18_DEFAULT,
                manage_slot_behavior: false,
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !client.is_streaming() && Instant::now() < deadline {
            if client.wants_write() {
                client.writable().unwrap();
            }
            client.readable(|_| Ok(())).unwrap();
            std::thread::yield_now();
        }
        assert!(
            client.is_streaming(),
            "TLS publisher handshake did not reach CopyBoth"
        );
        client.unbind();
        publisher.join().unwrap();
    }
}
