//! Bounded postgres_fdw transport and row materialization.

use core::fmt::Write as _;
use std::time::{Duration, Instant};

use crate::mem::arena::Arena;
use crate::pg::replication_client::{ClientError, ClientEvent, ConnectionInfo, SqlEvent, SslMode};
use crate::pg::respond::Responder;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::types::Datum;
use crate::sql_err;
use crate::storage::foreign::{ForeignMappingUser, ForeignServerDefinition};
use crate::storage::{
    ColumnSet, ForeignConnectionKey, ForeignSessionLease, ForeignSessionStart, PrivilegeSet,
    Storage, TableDef, rowenc,
};
use crate::util::StackStr;

const DEFAULT_CONNECT_TIMEOUT_SECONDS: u64 = 10;

/// A PostgreSQL heap tuple identity returned by `ctid::text`.  Parsing it at
/// the transport boundary prevents a remote row locator from being confused
/// with user SQL or an arbitrary parameter string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RemoteTupleId {
    block: u32,
    offset: u16,
}

impl RemoteTupleId {
    pub(crate) fn parse(text: &[u8]) -> Result<Self, SqlError> {
        let text = core::str::from_utf8(text).map_err(|_| {
            sql_err!(
                sqlstate::PROTOCOL_VIOLATION,
                "foreign ctid is not UTF-8 text"
            )
        })?;
        let Some(inner) = text
            .strip_prefix('(')
            .and_then(|value| value.strip_suffix(')'))
        else {
            return Err(sql_err!(
                sqlstate::PROTOCOL_VIOLATION,
                "foreign ctid has invalid syntax"
            ));
        };
        let Some((block, offset)) = inner.split_once(',') else {
            return Err(sql_err!(
                sqlstate::PROTOCOL_VIOLATION,
                "foreign ctid has invalid syntax"
            ));
        };
        if block.is_empty() || offset.is_empty() || block.contains(',') || offset.contains(',') {
            return Err(sql_err!(
                sqlstate::PROTOCOL_VIOLATION,
                "foreign ctid has invalid syntax"
            ));
        }
        let block = block.parse::<u32>().map_err(|_| {
            sql_err!(
                sqlstate::PROTOCOL_VIOLATION,
                "foreign ctid block is out of range"
            )
        })?;
        let offset = offset.parse::<u16>().map_err(|_| {
            sql_err!(
                sqlstate::PROTOCOL_VIOLATION,
                "foreign ctid offset is out of range"
            )
        })?;
        if offset == 0 {
            return Err(sql_err!(
                sqlstate::PROTOCOL_VIOLATION,
                "foreign ctid offset is zero"
            ));
        }
        Ok(Self { block, offset })
    }

    pub(crate) fn write_text<const N: usize>(self, output: &mut StackStr<N>) {
        let _ = write!(output, "({},{})", self.block, self.offset);
    }

    pub(crate) fn sort_key(self) -> u64 {
        (u64::from(self.block) << 16) | u64::from(self.offset)
    }
}

std::thread_local! {
    /// The command the foreign session is running, for the error context
    /// PostgreSQL's `pgfdw_report_error` adds.
    static REMOTE_COMMAND: core::cell::Cell<(*const u8, usize)> =
        const { core::cell::Cell::new((core::ptr::null(), 0)) };
}

/// The foreign command running until this scope ends.
struct RemoteCommandScope((*const u8, usize));

impl Drop for RemoteCommandScope {
    fn drop(&mut self) {
        REMOTE_COMMAND.with(|command| command.set(self.0));
    }
}

/// Names `command` as the one the foreign session runs. The scope is dropped
/// before `command`, which every caller declares first.
fn remote_command(command: &str) -> RemoteCommandScope {
    RemoteCommandScope(
        REMOTE_COMMAND.with(|current| current.replace((command.as_ptr(), command.len()))),
    )
}

/// A foreign server's error context: its own `CONTEXT`, then the command
/// that failed.
struct RemoteErrorFrame<'a> {
    context: Option<&'a str>,
    command: Option<&'a str>,
}

impl crate::sql::error_context::Frame for RemoteErrorFrame<'_> {
    fn write_line(&self, out: &mut dyn core::fmt::Write, _positioned: bool) -> core::fmt::Result {
        if let Some(context) = self.context {
            out.write_str(context)?;
            if self.command.is_some() {
                out.write_char('\n')?;
            }
        }
        if let Some(command) = self.command {
            write!(out, "remote SQL command: {command}")?;
        }
        Ok(())
    }
}

/// A foreign server's error as PostgreSQL's `pgfdw_report_error` re-raises
/// it: SQLSTATE, message, `DETAIL`, `HINT` and `CONTEXT`, then the remote
/// command; any other client failure is a local `HV000`.
fn client_error(error: ClientError) -> SqlError {
    match error {
        ClientError::Publisher(diagnostic) => {
            crate::sql::eval::stash_full_diagnostic(crate::sql::eval::Diagnostic {
                detail: diagnostic
                    .detail
                    .map(|detail| crate::util::StackStr::from_str(detail.as_str())),
                hint: diagnostic
                    .hint
                    .map(|hint| crate::util::StackStr::from_str(hint.as_str())),
                objects: crate::sql::eval::ErrorObjects::default(),
            });
            let (text, len) = REMOTE_COMMAND.with(core::cell::Cell::get);
            // SAFETY: a `RemoteCommandScope` names a command that outlives it.
            let command = (!text.is_null()).then(|| unsafe {
                core::str::from_utf8_unchecked(core::slice::from_raw_parts(text, len))
            });
            let frame = RemoteErrorFrame {
                context: diagnostic.context.as_ref().map(|context| context.as_str()),
                command,
            };
            match crate::sql::error_context::within(&frame, || {
                Err::<core::convert::Infallible, _>(SqlError {
                    sqlstate: diagnostic.sqlstate,
                    message: diagnostic.message,
                })
            }) {
                Err(error) => error,
                Ok(never) => match never {},
            }
        }
        error => sql_err!(
            sqlstate::FDW_ERROR,
            "foreign PostgreSQL connection: {}",
            error
        ),
    }
}

/// Decodes one field at the foreign-row boundary into the physical datum
/// that the local executor may safely encode. A PostgreSQL composite arrives
/// as a transient record; coercing through the declared column makes its
/// durable representation explicit before it can reach `rowenc`.
pub(super) fn decode_foreign_field<'a>(
    storage: &Storage,
    table: &TableDef,
    column: usize,
    raw: Option<&'a [u8]>,
    txid: u32,
    arena: &'a Arena,
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<Datum<'a>, SqlError> {
    let declared = storage.declared_column_type(&table.columns()[column], txid)?;
    let value = match raw {
        Some(raw) => super::exec::decode_text_input(
            storage,
            declared.catalog_oid(),
            raw,
            arena,
            txid,
            sequences,
        )?,
        None => super::exec::coerce_binary_input_null(
            storage,
            declared.catalog_oid(),
            arena,
            txid,
            sequences,
        )?,
    };
    if value.is_null() {
        return Ok(value);
    }
    super::exec::coerce(
        value,
        &table.columns()[column],
        storage,
        txid,
        arena,
        sequences,
    )
}

fn poll_client(
    client: &mut crate::pg::replication_client::ReplicationClient,
    deadline: Instant,
    visit: &mut impl FnMut(ClientEvent<'_>) -> Result<(), ClientError>,
) -> Result<(), SqlError> {
    let mut descriptor = libc::pollfd {
        fd: client.raw_fd(),
        events: libc::POLLIN
            | if client.wants_write() {
                libc::POLLOUT
            } else {
                0
            },
        revents: 0,
    };
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(timed_out());
        }
        let timeout = remaining
            .as_nanos()
            .div_ceil(1_000_000)
            .min(i32::MAX as u128) as i32;
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout) };
        if result < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if result < 0 {
            return Err(client_error(ClientError::Io(
                std::io::Error::last_os_error(),
            )));
        }
        if result == 0 {
            return Err(timed_out());
        }
        break;
    }
    if descriptor.revents & libc::POLLOUT != 0 {
        client.writable().map_err(client_error)?;
    }
    if descriptor.revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
        // A closed peer reads as end of file after any frames it sent.
        client.readable(usize::MAX, visit).map_err(client_error)?;
    } else if descriptor.revents & libc::POLLNVAL != 0 {
        return Err(lost_connection());
    }
    Ok(())
}

fn timed_out() -> SqlError {
    sql_err!(
        sqlstate::QUERY_CANCELED,
        "foreign PostgreSQL operation timed out"
    )
}

/// libpq's report when the server's socket closes.
fn lost_connection() -> SqlError {
    sql_err!(
        sqlstate::CONNECTION_FAILURE,
        "server closed the connection unexpectedly"
    )
}

fn malformed() -> ClientError {
    ClientError::Protocol(crate::pg::replication_client::FrameError::Malformed)
}

/// A foreign server and the identity its cached connection has.
#[derive(Clone, Copy)]
struct RemoteServer {
    endpoint: ConnectionInfo,
    timeout: Duration,
    key: ForeignConnectionKey,
    keep_connections: bool,
}

/// Reads until a response ends after a failed exchange, so the connection
/// can roll back. A transport failure or timeout instead marks the session
/// broken: its remote transaction is lost.
fn settle(client: &mut crate::storage::ForeignClientGuard<'_>, failure: &SqlError) {
    if failure.sqlstate == sqlstate::QUERY_CANCELED
        || failure.sqlstate == sqlstate::CONNECTION_FAILURE
        || !client.is_bound()
    {
        client.break_session();
        return;
    }
    let deadline = Instant::now() + client.timeout();
    let mut ignore = |event: ClientEvent<'_>| match event {
        ClientEvent::Sql(_) => Ok(()),
        ClientEvent::Replication(_) => Err(malformed()),
    };
    while client.is_awaiting_sql() {
        // The failed read may have left the rest of the response buffered.
        let drained = if client.has_buffered_frame() {
            client
                .readable(usize::MAX, &mut ignore)
                .map_err(client_error)
        } else {
            poll_client(client, deadline, &mut ignore)
        };
        if drained.is_err() {
            client.break_session();
            return;
        }
    }
    if !client.is_sql_ready() {
        client.break_session();
    }
}

/// Sends one command on a session in this transaction and reads its whole
/// response, handing every event but the closing ReadyForQuery to `visit`.
/// A remote error is reported with `reported` as its `remote SQL command`.
/// Returns the remote transaction status.
fn exchange(
    storage: &Storage,
    lease: ForeignSessionLease,
    command: &str,
    parameters: Option<&[Option<&[u8]>]>,
    reported: &str,
    visit: &mut dyn FnMut(SqlEvent<'_>) -> Result<(), SqlError>,
) -> Result<u8, SqlError> {
    let mut client = storage.foreign_client(lease)?;
    if client.is_broken() {
        return Err(lost_connection());
    }
    let deadline = Instant::now() + client.timeout();
    let _remote = remote_command(reported);
    let sent = match parameters {
        Some(parameters) => client.query_params(command, parameters),
        None => client.query(command),
    };
    sent.map_err(client_error)?;
    let mut status = None;
    let mut failure = None;
    while status.is_none() {
        let polled = poll_client(&mut client, deadline, &mut |event| {
            match event {
                ClientEvent::Sql(SqlEvent::Ready { transaction_status }) => {
                    status = Some(transaction_status);
                }
                ClientEvent::Sql(event) => {
                    if failure.is_none()
                        && let Err(error) = visit(event)
                    {
                        failure = Some(error);
                    }
                }
                ClientEvent::Replication(_) => return Err(malformed()),
            }
            Ok(())
        });
        if let Err(error) = polled {
            settle(&mut client, &error);
            return Err(error);
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(status.expect("the loop ends at ReadyForQuery")),
    }
}

/// Runs a command whose only response is its command tag.
fn command(
    storage: &Storage,
    lease: ForeignSessionLease,
    command: &str,
    tag: &str,
) -> Result<u8, SqlError> {
    exchange(
        storage,
        lease,
        command,
        None,
        command,
        &mut |event| match event {
            SqlEvent::CommandComplete { tag: received } if received == tag => Ok(()),
            _ => Err(unexpected_response(command)),
        },
    )
}

fn unexpected_response(command: &str) -> SqlError {
    sql_err!(
        sqlstate::PROTOCOL_VIOLATION,
        "unexpected foreign PostgreSQL response to {}",
        command.split(' ').next().unwrap_or(command)
    )
}

/// Opens `server`'s connection and authenticates.
fn connect(
    client: &mut crate::storage::ForeignClientGuard<'_>,
    endpoint: ConnectionInfo,
    timeout: Duration,
) -> Result<(), SqlError> {
    client.unbind();
    client.bind_sql(endpoint).map_err(client_error)?;
    let deadline = Instant::now() + timeout;
    let mut ready = false;
    while !ready {
        poll_client(client, deadline, &mut |event| {
            match event {
                ClientEvent::Sql(SqlEvent::Ready {
                    transaction_status: b'I',
                }) => ready = true,
                _ => return Err(malformed()),
            }
            Ok(())
        })?;
    }
    Ok(())
}

/// postgres_fdw's `configure_remote_session` and `begin_remote_xact`: a
/// search path that qualifies every remote name, the value formats it reads,
/// then a transaction that mirrors SERIALIZABLE and otherwise uses
/// REPEATABLE READ, keeping one remote snapshot across local statements.
fn begin(
    storage: &Storage,
    lease: ForeignSessionLease,
    txid: u32,
    configure: bool,
) -> Result<(), SqlError> {
    if configure {
        for setting in [
            "SET search_path = pg_catalog",
            "SET timezone = 'UTC'",
            "SET datestyle = ISO",
            "SET intervalstyle = postgres",
            "SET extra_float_digits = 3",
        ] {
            if command(storage, lease, setting, "SET")? != b'I' {
                return Err(unexpected_response(setting));
            }
        }
    }
    let statement_context = storage.foreign_statement_context_for(txid);
    let begin = if statement_context.is_serializable() {
        "START TRANSACTION ISOLATION LEVEL SERIALIZABLE"
    } else {
        "START TRANSACTION ISOLATION LEVEL REPEATABLE READ"
    };
    if command(storage, lease, begin, "START TRANSACTION")? != b'T' {
        return Err(unexpected_response(begin));
    }
    // The remote transaction opens at first use. Recreate the savepoints
    // made before it so later RELEASE and ROLLBACK TO have the same shape
    // locally and remotely.
    for name in statement_context.savepoints() {
        let mut savepoint = StackStr::<256>::new();
        let _ = savepoint.write_str("SAVEPOINT ");
        quote_identifier(&mut savepoint, name.as_str());
        if savepoint.is_truncated() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "foreign savepoint command exceeds its fixed buffer"
            ));
        }
        command(storage, lease, savepoint.as_str(), "SAVEPOINT")?;
    }
    Ok(())
}

/// Enters the remote transaction of `txid` on `server`'s cached connection,
/// opening the connection when the cache holds none. Storage reserves the
/// fixed slot first, so no other backend can take its buffers meanwhile.
fn enter_session(
    storage: &Storage,
    txid: u32,
    server: &RemoteServer,
) -> Result<ForeignSessionLease, SqlError> {
    let (lease, start) = storage.reserve_foreign_session(
        txid,
        server.key,
        server.endpoint,
        server.timeout,
        server.keep_connections,
    )?;
    let open = |storage: &Storage| -> Result<(), SqlError> {
        connect(
            &mut storage.foreign_client(lease)?,
            server.endpoint,
            server.timeout,
        )?;
        begin(storage, lease, txid, true)
    };
    let result = match start {
        ForeignSessionStart::Active => return Ok(lease),
        ForeignSessionStart::Connect => open(storage),
        ForeignSessionStart::Begin => match begin(storage, lease, txid, false) {
            // A cached connection found broken outside every transaction
            // is reopened once, as PostgreSQL's GetConnection does.
            Err(error) if error.sqlstate == sqlstate::CONNECTION_FAILURE => open(storage),
            result => result,
        },
    };
    match result {
        Ok(()) => {
            storage.foreign_client(lease)?.activate();
            Ok(lease)
        }
        Err(error) => {
            storage.clear_foreign_session(lease);
            Err(error)
        }
    }
}

/// Ends one remote transaction with `command`, keeping the connection
/// cached when it returns to idle.
fn end_remote_transaction(
    storage: &Storage,
    lease: ForeignSessionLease,
    command_text: &str,
    tag: &str,
) -> Result<(), SqlError> {
    let ended = command(storage, lease, command_text, tag);
    let client = storage.foreign_client(lease)?;
    // PostgreSQL discards a connection whose transaction did not end
    // cleanly, a failed COMMIT included.
    if matches!(ended, Ok(b'I')) {
        client.finish_transaction();
    } else {
        client.close();
    }
    match ended {
        Ok(b'I') => Ok(()),
        Ok(_) => Err(unexpected_response(command_text)),
        Err(error) => Err(error),
    }
}

/// Commits every remote transaction `txid` opened. After the first failure
/// the rest roll back, and the error aborts the local transaction.
pub(crate) fn commit_session(storage: &Storage, txid: u32) -> Result<(), SqlError> {
    let mut result = Ok(());
    for lease in storage.foreign_sessions(txid) {
        if result.is_ok() {
            result = end_remote_transaction(storage, lease, "COMMIT TRANSACTION", "COMMIT");
        } else {
            abort_one(storage, lease);
        }
    }
    result
}

fn abort_one(storage: &Storage, lease: ForeignSessionLease) {
    match storage.foreign_client(lease) {
        Ok(client) if client.is_broken() => client.close(),
        Ok(client) => {
            drop(client);
            let _ = end_remote_transaction(storage, lease, "ABORT TRANSACTION", "ROLLBACK");
        }
        Err(_) => {}
    }
}

/// ROLLBACK has no error channel in PostgreSQL's command protocol. A
/// connection that cannot roll back cleanly is closed, which aborts its
/// remote transaction.
pub(crate) fn abort_session(storage: &Storage, txid: u32) {
    for lease in storage.foreign_sessions(txid) {
        abort_one(storage, lease);
    }
}

fn savepoint_command(
    storage: &Storage,
    txid: u32,
    prefix: &str,
    name: &str,
) -> Result<(), SqlError> {
    let mut text = StackStr::<256>::new();
    let _ = text.write_str(prefix);
    quote_identifier(&mut text, name);
    if text.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "foreign savepoint command exceeds its fixed buffer"
        ));
    }
    let tag = match prefix {
        "SAVEPOINT " => "SAVEPOINT",
        "RELEASE SAVEPOINT " => "RELEASE",
        "ROLLBACK TO SAVEPOINT " => "ROLLBACK",
        _ => unreachable!("foreign savepoint command has a fixed prefix"),
    };
    for lease in storage.foreign_sessions(txid) {
        let broken = storage.foreign_client(lease)?.is_broken();
        // A lost connection stays broken until the transaction ends; its
        // rollback to a savepoint has nothing left to undo remotely.
        if broken && tag == "ROLLBACK" {
            continue;
        }
        if command(storage, lease, text.as_str(), tag)? != b'T' {
            return Err(unexpected_response(text.as_str()));
        }
    }
    Ok(())
}

pub(crate) fn savepoint(storage: &Storage, txid: u32, name: &str) -> Result<(), SqlError> {
    savepoint_command(storage, txid, "SAVEPOINT ", name)
}

pub(crate) fn release_savepoint(storage: &Storage, txid: u32, name: &str) -> Result<(), SqlError> {
    savepoint_command(storage, txid, "RELEASE SAVEPOINT ", name)
}

pub(crate) fn rollback_to_savepoint(
    storage: &Storage,
    txid: u32,
    name: &str,
) -> Result<(), SqlError> {
    savepoint_command(storage, txid, "ROLLBACK TO SAVEPOINT ", name)
}

/// Closes a disconnecting backend's cached connections.
pub(crate) fn drop_backend_connections(storage: &Storage, backend: i32) {
    storage.drop_foreign_connections(backend);
}

/// PostgreSQL's `quote_identifier`, which postgres_fdw's deparser uses for
/// every remote name: bare when it round-trips, otherwise double-quoted.
fn quote_identifier(output: &mut impl core::fmt::Write, value: &str) {
    if !crate::sql::eval::ident_needs_quotes(value) {
        let _ = output.write_str(value);
        return;
    }
    let _ = output.write_char('"');
    for character in value.chars() {
        if character == '"' {
            let _ = output.write_char('"');
        }
        let _ = output.write_char(character);
    }
    let _ = output.write_char('"');
}

fn quote_remote_column(
    output: &mut impl core::fmt::Write,
    foreign: &crate::storage::foreign::ForeignTableDefinition,
    column: usize,
    definition: &crate::storage::ColumnMeta,
) {
    if let Some(option) = foreign
        .column_options
        .options_for(column as u16)
        .find(|option| option.name.as_str() == "column_name")
    {
        quote_identifier(output, option.value.as_str());
    } else {
        quote_identifier(output, definition.name.as_str());
    }
}

fn quote_literal<const N: usize>(output: &mut StackStr<N>, value: &str) {
    let _ = output.write_char('\'');
    for character in value.chars() {
        if character == '\'' {
            let _ = output.write_char('\'');
        }
        let _ = output.write_char(character);
    }
    let _ = output.write_char('\'');
}

#[derive(Clone, Copy)]
pub(crate) struct ImportCommand<'a> {
    pub(crate) sql: &'a str,
}

fn selected_for_import(
    name: &str,
    partition: bool,
    selection: crate::sql::ast::ForeignSchemaSelection<'_>,
) -> bool {
    match selection {
        crate::sql::ast::ForeignSchemaSelection::All => !partition,
        crate::sql::ast::ForeignSchemaSelection::LimitTo(names) => names.contains(&name),
        crate::sql::ast::ForeignSchemaSelection::Except(names) => {
            !partition && !names.contains(&name)
        }
    }
}

pub(crate) fn import_commands<'a>(
    storage: &Storage,
    command: &crate::sql::ast::ImportForeignSchema<'_>,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a [ImportCommand<'a>], SqlError> {
    let Some((server_slot, _)) = storage.foreign_server(command.server, txid) else {
        return Err(sql_err!(
            sqlstate::UNDEFINED_OBJECT,
            "server \"{}\" does not exist",
            command.server
        ));
    };
    let (server, _) = connection_for_server(storage, server_slot, txid)?;
    let import_collate = command
        .options
        .iter()
        .find(|option| option.name.eq_ignore_ascii_case("import_collate"))
        .map_or(Ok(true), |option| super::eval::parse_bool(option.value))?;
    let import_default = command
        .options
        .iter()
        .find(|option| option.name.eq_ignore_ascii_case("import_default"))
        .map_or(Ok(false), |option| super::eval::parse_bool(option.value))?;
    let import_generated = command
        .options
        .iter()
        .find(|option| option.name.eq_ignore_ascii_case("import_generated"))
        .map_or(Ok(true), |option| super::eval::parse_bool(option.value))?;
    let import_not_null = command
        .options
        .iter()
        .find(|option| option.name.eq_ignore_ascii_case("import_not_null"))
        .map_or(Ok(true), |option| super::eval::parse_bool(option.value))?;

    let mut query = StackStr::<16_384>::new();
    let _ = query.write_str(
        "SELECT c.relname, COALESCE(string_agg(quote_ident(a.attname) || ' ' || \
         pg_catalog.format_type(a.atttypid, a.atttypmod)",
    );
    if import_collate {
        let _ = query.write_str(
            " || CASE WHEN a.attcollation <> 0 THEN ' COLLATE ' || \
             quote_ident(cn.nspname) || '.' || quote_ident(co.collname) ELSE '' END",
        );
    }
    if import_default {
        let _ = query.write_str(
            " || CASE WHEN ad.adbin IS NOT NULL AND a.attgenerated = '' THEN ' DEFAULT ' || \
             pg_catalog.pg_get_expr(ad.adbin, ad.adrelid) ELSE '' END",
        );
    }
    if import_generated {
        let _ = query.write_str(
            " || CASE WHEN a.attgenerated <> '' THEN ' GENERATED ALWAYS AS (' || \
             pg_catalog.pg_get_expr(ad.adbin, ad.adrelid) || ') STORED' ELSE '' END",
        );
    }
    if import_not_null {
        let _ = query.write_str(" || CASE WHEN a.attnotnull THEN ' NOT NULL' ELSE '' END");
    }
    let _ = query.write_str(
        ", ', ' ORDER BY a.attnum) FILTER (WHERE a.attnum IS NOT NULL), ''), \
         c.relispartition::text FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 \
         AND NOT a.attisdropped LEFT JOIN pg_catalog.pg_attrdef ad ON ad.adrelid = c.oid \
         AND ad.adnum = a.attnum LEFT JOIN pg_catalog.pg_collation co ON co.oid = a.attcollation \
         LEFT JOIN pg_catalog.pg_namespace cn ON cn.oid = co.collnamespace WHERE n.nspname = ",
    );
    quote_literal(&mut query, command.remote_schema);
    let _ = query.write_str(
        " AND c.relkind IN ('r','v','m','f','p') GROUP BY c.oid, c.relname, \
         c.relispartition ORDER BY c.relname",
    );
    if query.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "foreign import query exceeds its fixed buffer"
        ));
    }

    let mut commands: *mut ImportCommand<'a> = core::ptr::null_mut();
    let mut count = 0usize;
    let mut capacity = 0usize;
    let lease = enter_session(storage, txid, &server)?;
    exchange(
        storage,
        lease,
        query.as_str(),
        None,
        query.as_str(),
        &mut |event| {
            let row = match event {
                SqlEvent::RowDescription { fields } if fields != 3 => {
                    return Err(sql_err!(
                        sqlstate::PROTOCOL_VIOLATION,
                        "foreign import returned {} columns, expected 3",
                        fields
                    ));
                }
                SqlEvent::RowDescription { .. } | SqlEvent::CommandComplete { .. } => {
                    return Ok(());
                }
                SqlEvent::DataRow(row) => row,
                _ => return Err(unexpected_response("IMPORT")),
            };
            let [Some(name), Some(column_sql), Some(partition)] = row.columns() else {
                return Err(sql_err!(
                    sqlstate::PROTOCOL_VIOLATION,
                    "foreign import returned an invalid catalog row"
                ));
            };
            let partition = match *partition {
                b"t" | b"true" => true,
                b"f" | b"false" => false,
                _ => {
                    return Err(sql_err!(
                        sqlstate::PROTOCOL_VIOLATION,
                        "foreign import returned an invalid partition flag"
                    ));
                }
            };
            let name = core::str::from_utf8(name).map_err(|_| {
                sql_err!(
                    sqlstate::CHARACTER_NOT_IN_REPERTOIRE,
                    "foreign import returned a non-UTF8 relation name"
                )
            })?;
            let column_sql = core::str::from_utf8(column_sql).map_err(|_| {
                sql_err!(
                    sqlstate::CHARACTER_NOT_IN_REPERTOIRE,
                    "foreign import returned a non-UTF8 table definition"
                )
            })?;
            if !selected_for_import(name, partition, command.selection) {
                return Ok(());
            }
            let mut sql = StackStr::<16_384>::new();
            let _ = sql.write_str("CREATE FOREIGN TABLE ");
            quote_identifier(&mut sql, command.local_schema);
            let _ = sql.write_char('.');
            quote_identifier(&mut sql, name);
            let _ = sql.write_str(" (");
            let _ = sql.write_str(column_sql);
            let _ = sql.write_str(") SERVER ");
            quote_identifier(&mut sql, command.server);
            let _ = sql.write_str(" OPTIONS (schema_name ");
            quote_literal(&mut sql, command.remote_schema);
            let _ = sql.write_str(", table_name ");
            quote_literal(&mut sql, name);
            let _ = sql.write_char(')');
            checked(&sql, "imported foreign-table definition")?;
            let sql = arena
                .alloc_str(sql.as_str())
                .map_err(|_| super::eval::arena_full())?;
            const EMPTY: ImportCommand<'static> = ImportCommand { sql: "" };
            if count == capacity {
                let next = if capacity == 0 { 8 } else { capacity * 2 };
                let fresh = arena
                    .alloc_slice_with(next, |_| EMPTY)
                    .map_err(|_| super::eval::arena_full())?;
                if count != 0 {
                    let prior = unsafe { core::slice::from_raw_parts(commands, count) };
                    fresh[..count].copy_from_slice(prior);
                }
                commands = fresh.as_mut_ptr();
                capacity = next;
            }
            unsafe { commands.add(count).write(ImportCommand { sql }) };
            count += 1;
            Ok(())
        },
    )?;
    Ok(if count == 0 {
        &[]
    } else {
        unsafe { core::slice::from_raw_parts(commands, count) }
    })
}
fn connection_for_server(
    storage: &Storage,
    server_slot: usize,
    txid: u32,
) -> Result<(RemoteServer, ForeignServerDefinition), SqlError> {
    let server = storage
        .foreign_server_by_slot(server_slot, txid)
        .ok_or_else(|| sql_err!(sqlstate::UNDEFINED_OBJECT, "foreign server does not exist"))?;
    let wrapper = storage
        .foreign_wrapper_by_slot(server.wrapper as usize, txid)
        .ok_or_else(|| sql_err!(sqlstate::FDW_ERROR, "foreign-data wrapper does not exist"))?;
    wrapper.require_handler()?;
    let role = storage.current_role_slot(txid).ok_or_else(|| {
        sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "current role does not exist"
        )
    })?;
    if !storage.has_object_privilege(
        crate::storage::AccessObject {
            class: crate::storage::AccessClass::ForeignServer,
            slot: server_slot as u16,
        },
        role,
        PrivilegeSet::USAGE,
        txid,
    ) {
        return Err(sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "permission denied for server {}",
            server.name.as_str()
        ));
    }
    let (mapping_slot, mapping) = storage
        .foreign_user_mapping(
            server_slot as u16,
            ForeignMappingUser::Role(role as u16),
            txid,
        )
        .or_else(|| {
            storage.foreign_user_mapping(server_slot as u16, ForeignMappingUser::Public, txid)
        })
        .ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "user mapping not found for foreign server \"{}\"",
                server.name.as_str()
            )
        })?;

    let host = match (server.options.get("hostaddr"), server.options.get("host")) {
        (Some(address), Some(host)) if address == host => address,
        (Some(_), Some(_)) => {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "distinct host and hostaddr values are not supported"
            ));
        }
        (Some(address), None) => address,
        (None, Some(host)) => host,
        (None, None) => {
            return Err(sql_err!(
                sqlstate::FDW_ERROR,
                "foreign server requires host or hostaddr"
            ));
        }
    };
    let port = server
        .options
        .get("port")
        .or_else(|| environment("PGPORT"))
        .unwrap_or("5432")
        .parse::<u16>()
        .map_err(|_| sql_err!(sqlstate::FDW_ERROR, "invalid foreign server port"))?;
    // libpq's defaults: the user from PGUSER or the server process's
    // operating-system user, the database from PGDATABASE or the user.
    let default_user = libpq_default_user()?;
    let user = mapping.options.get("user").unwrap_or(default_user.as_str());
    let database = server
        .options
        .get("dbname")
        .or_else(|| environment("PGDATABASE"))
        .unwrap_or(user);
    let database = crate::storage::SqlName::parse(database).map_err(|_| {
        sql_err!(
            sqlstate::FDW_INVALID_ATTRIBUTE_VALUE,
            "invalid foreign database name"
        )
    })?;
    let password = mapping.options.get("password");
    let application_name = server.options.get("application_name").unwrap_or("pos3ql");
    let ssl_mode = match server.options.get("sslmode").unwrap_or("prefer") {
        "disable" => SslMode::Disable,
        "prefer" => SslMode::Prefer,
        "require" => SslMode::Require,
        value => {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "foreign sslmode \"{}\" is not supported",
                value
            ));
        }
    };
    let timeout = server
        .options
        .get("connect_timeout")
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|_| sql_err!(sqlstate::FDW_ERROR, "invalid foreign connect_timeout"))?
        .unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECONDS);
    if timeout == 0 || timeout > 300 {
        return Err(sql_err!(
            sqlstate::FDW_ERROR,
            "foreign connect_timeout must be between 1 and 300 seconds"
        ));
    }
    let endpoint = ConnectionInfo::for_foreign(
        host,
        port,
        user,
        database.as_str(),
        password,
        application_name,
        ssl_mode,
    )
    .map_err(|error| {
        sql_err!(
            sqlstate::FDW_ERROR,
            "invalid foreign PostgreSQL endpoint: {:?}",
            error
        )
    })?;
    let keep_connections = server
        .options
        .get("keep_connections")
        .map_or(Ok(true), |value| option_boolean("keep_connections", value))?;
    let key = ForeignConnectionKey {
        owner: storage.current_connection_id(),
        server: server_slot as u16,
        server_created_at: storage.foreign_server_entry(server_slot).created_at,
        mapping: mapping_slot as u16,
        mapping_created_at: storage.foreign_mapping_entry(mapping_slot).created_at,
    };
    Ok((
        RemoteServer {
            endpoint,
            timeout: Duration::from_secs(timeout),
            key,
            keep_connections,
        },
        server,
    ))
}

/// A libpq environment default, read without allocating.
fn environment(name: &'static str) -> Option<&'static str> {
    let mut key = [0u8; 16];
    key[..name.len()].copy_from_slice(name.as_bytes());
    // SAFETY: `key` is NUL-terminated; the server never sets its own
    // environment after startup, so the returned string stays valid.
    let value = unsafe { libc::getenv(key.as_ptr().cast()) };
    if value.is_null() {
        return None;
    }
    let value = unsafe { core::ffi::CStr::from_ptr(value) };
    value.to_str().ok().filter(|value| !value.is_empty())
}

/// libpq's default user: PGUSER, else the effective operating-system user.
fn libpq_default_user() -> Result<StackStr<64>, SqlError> {
    if let Some(user) = environment("PGUSER") {
        return Ok(StackStr::from_str(user));
    }
    let mut entry: libc::passwd = unsafe { core::mem::zeroed() };
    let mut buffer = [0 as libc::c_char; 1024];
    let mut result = core::ptr::null_mut();
    // SAFETY: every pointer names a live buffer of the stated length.
    let status = unsafe {
        libc::getpwuid_r(
            libc::geteuid(),
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() {
        return Err(sql_err!(
            sqlstate::FDW_ERROR,
            "could not look up the server's operating-system user"
        ));
    }
    let name = unsafe { core::ffi::CStr::from_ptr(entry.pw_name) };
    Ok(StackStr::from_str(name.to_str().map_err(|_| {
        sql_err!(
            sqlstate::FDW_ERROR,
            "the server's operating-system user name is not UTF-8"
        )
    })?))
}

/// PostgreSQL's `defGetBoolean` for an option string.
pub(crate) fn option_boolean(name: &str, value: &str) -> Result<bool, SqlError> {
    if value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("on") {
        Ok(true)
    } else if value.eq_ignore_ascii_case("false") || value.eq_ignore_ascii_case("off") {
        Ok(false)
    } else {
        Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "{} requires a Boolean value",
            name
        ))
    }
}

/// postgres_fdw's `fetch_size` and `batch_size`: the table's option, else
/// the server's, else `default`. The validator already accepted the value.
fn integer_option(
    foreign: &crate::storage::foreign::ForeignTableDefinition,
    server: &ForeignServerDefinition,
    name: &str,
    default: i32,
) -> i32 {
    foreign
        .options
        .get(name)
        .or_else(|| server.options.get(name))
        .and_then(crate::sql::guc::parse_unitless_int)
        .unwrap_or(default)
}

/// A foreign table's remote relation and connection.
struct RemoteTable {
    server: RemoteServer,
    server_definition: ForeignServerDefinition,
    table: TableDef,
    foreign: crate::storage::foreign::ForeignTableDefinition,
}

impl RemoteTable {
    fn open(storage: &Storage, table_slot: usize, txid: u32) -> Result<Self, SqlError> {
        let table = storage.table_def(table_slot, txid).clone();
        let foreign = storage
            .foreign_table(table_slot as u16, txid)
            .ok_or_else(|| sql_err!(sqlstate::FDW_ERROR, "foreign table has no server binding"))?
            .1;
        let (server, server_definition) =
            connection_for_server(storage, foreign.server as usize, txid)?;
        Ok(Self {
            server,
            server_definition,
            table,
            foreign,
        })
    }

    /// postgres_fdw's `deparseRelation`: `schema.table`, from the table's
    /// options when it has them.
    fn write_relation(&self, output: &mut impl core::fmt::Write) {
        let schema = self
            .foreign
            .options
            .get("schema_name")
            .unwrap_or(self.table.schema.as_str());
        let name = self
            .foreign
            .options
            .get("table_name")
            .unwrap_or(self.table.name.as_str());
        quote_identifier(output, schema);
        let _ = output.write_char('.');
        quote_identifier(output, name);
    }

    fn write_column(&self, output: &mut impl core::fmt::Write, column: usize) {
        quote_remote_column(output, &self.foreign, column, &self.table.columns()[column]);
    }

    /// postgres_fdw's `deparseSelectSql` for a scan that reads every column.
    fn write_select<const N: usize>(&self, output: &mut StackStr<N>) {
        let _ = output.write_str("SELECT ");
        for column in 0..self.table.n_columns {
            if column != 0 {
                let _ = output.write_str(", ");
            }
            self.write_column(output, column);
        }
        if self.table.n_columns == 0 {
            let _ = output.write_str("NULL");
        }
        let _ = output.write_str(" FROM ");
        self.write_relation(output);
    }

    fn fetch_size(&self) -> i32 {
        integer_option(&self.foreign, &self.server_definition, "fetch_size", 100)
    }

    fn batch_size(&self) -> i32 {
        integer_option(&self.foreign, &self.server_definition, "batch_size", 1)
    }
}

fn checked<const N: usize>(text: &StackStr<N>, what: &str) -> Result<(), SqlError> {
    if text.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "foreign {} exceeds its fixed buffer",
            what
        ));
    }
    Ok(())
}

/// Reads `query` through a cursor as postgres_fdw does: `DECLARE cN CURSOR
/// FOR`, then `FETCH fetch_size FROM cN` until a short batch, then `CLOSE`.
/// A remote error names `query`, not the cursor command.
fn scan_cursor(
    storage: &Storage,
    lease: ForeignSessionLease,
    query: &str,
    fetch_size: i32,
    fields: usize,
    visit: &mut dyn FnMut(crate::pg::replication_client::SqlDataRow<'_>) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    let cursor = storage.foreign_client(lease)?.next_cursor_number();
    let mut declare = StackStr::<16_448>::new();
    let _ = write!(declare, "DECLARE c{cursor} CURSOR FOR\n{query}");
    checked(&declare, "cursor declaration")?;
    exchange(
        storage,
        lease,
        declare.as_str(),
        Some(&[]),
        query,
        &mut |event| match event {
            SqlEvent::CommandComplete {
                tag: "DECLARE CURSOR",
            } => Ok(()),
            _ => Err(unexpected_response("DECLARE")),
        },
    )?;
    let mut fetch = StackStr::<64>::new();
    let _ = write!(fetch, "FETCH {fetch_size} FROM c{cursor}");
    loop {
        let mut rows = 0i64;
        exchange(
            storage,
            lease,
            fetch.as_str(),
            None,
            query,
            &mut |event| match event {
                SqlEvent::RowDescription { fields: received } if received as usize != fields => {
                    Err(sql_err!(
                        sqlstate::PROTOCOL_VIOLATION,
                        "foreign scan returned {} columns, expected {}",
                        received,
                        fields
                    ))
                }
                SqlEvent::RowDescription { .. } | SqlEvent::CommandComplete { .. } => Ok(()),
                SqlEvent::DataRow(row) => {
                    if row.columns().len() != fields {
                        return Err(sql_err!(
                            sqlstate::PROTOCOL_VIOLATION,
                            "foreign scan returned an invalid row shape"
                        ));
                    }
                    rows += 1;
                    visit(row)
                }
                _ => Err(unexpected_response("FETCH")),
            },
        )?;
        if rows < i64::from(fetch_size) {
            break;
        }
    }
    let mut close = StackStr::<32>::new();
    let _ = write!(close, "CLOSE c{cursor}");
    command(storage, lease, close.as_str(), "CLOSE CURSOR")?;
    Ok(())
}

/// Appends one arena-owned row to an arena-grown array.
fn push_row<'a>(
    arena: &'a Arena,
    rows: &mut *mut &'a [u8],
    count: &mut usize,
    capacity: &mut usize,
    row: &'a [u8],
) -> Result<(), SqlError> {
    const EMPTY: &[u8] = &[];
    if *count == *capacity {
        let next = if *capacity == 0 { 8 } else { *capacity * 2 };
        let fresh = arena
            .alloc_slice_with(next, |_| EMPTY)
            .map_err(|_| super::eval::arena_full())?;
        if *count != 0 {
            let prior = unsafe { core::slice::from_raw_parts(*rows, *count) };
            fresh[..*count].copy_from_slice(prior);
        }
        *rows = fresh.as_mut_ptr();
        *capacity = next;
    }
    unsafe { rows.add(*count).write(row) };
    *count += 1;
    Ok(())
}

pub(crate) fn materialize<'a>(
    storage: &'a Storage,
    table_slot: usize,
    txid: u32,
    arena: &'a Arena,
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<&'a [&'a [u8]], SqlError> {
    let remote = RemoteTable::open(storage, table_slot, txid)?;
    let mut query = StackStr::<16_384>::new();
    remote.write_select(&mut query);
    checked(&query, "query")?;
    let table = &remote.table;
    let mut rows: *mut &[u8] = core::ptr::null_mut();
    let mut row_count = 0usize;
    let mut row_capacity = 0usize;
    let lease = enter_session(storage, txid, &remote.server)?;
    scan_cursor(
        storage,
        lease,
        query.as_str(),
        remote.fetch_size(),
        table.n_columns.max(1),
        &mut |remote_row| {
            let mut values = [Datum::Null; crate::storage::MAX_COLUMNS];
            for (column, raw) in remote_row.columns()[..table.n_columns].iter().enumerate() {
                values[column] =
                    decode_foreign_field(storage, table, column, *raw, txid, arena, sequences)?;
            }
            let encoded = super::exec::encode_projected_pub(&values[..table.n_columns], arena)?;
            push_row(arena, &mut rows, &mut row_count, &mut row_capacity, encoded)
        },
    )?;
    Ok(if row_count == 0 {
        &[]
    } else {
        unsafe { core::slice::from_raw_parts(rows, row_count) }
    })
}

/// Visits every remote row of an UPDATE or DELETE target with its parsed
/// PostgreSQL tuple identity, read as postgres_fdw reads a result relation:
/// every column and `ctid`, locked `FOR UPDATE`. The identity never becomes
/// a fabricated local heap row identifier.
pub(crate) fn visit_mutable_rows<'arena>(
    storage: &Storage,
    table_slot: usize,
    txid: u32,
    arena: &'arena Arena,
    visit: &mut impl FnMut(RemoteTupleId, &'arena [u8]) -> Result<(), SqlError>,
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<(), SqlError> {
    let remote = RemoteTable::open(storage, table_slot, txid)?;
    let table = &remote.table;
    let mut query = StackStr::<16_384>::new();
    let _ = query.write_str("SELECT ");
    for column in 0..table.n_columns {
        remote.write_column(&mut query, column);
        let _ = query.write_str(", ");
    }
    let _ = query.write_str("ctid FROM ");
    remote.write_relation(&mut query);
    let _ = query.write_str(" FOR UPDATE");
    checked(&query, "query")?;
    let lease = enter_session(storage, txid, &remote.server)?;
    scan_cursor(
        storage,
        lease,
        query.as_str(),
        remote.fetch_size(),
        table.n_columns + 1,
        &mut |row| {
            let tuple_id = match row.columns()[table.n_columns] {
                Some(ctid) => RemoteTupleId::parse(ctid)?,
                None => {
                    return Err(sql_err!(
                        sqlstate::PROTOCOL_VIOLATION,
                        "foreign mutable-row scan returned a NULL ctid"
                    ));
                }
            };
            let mut values = [Datum::Null; crate::storage::MAX_COLUMNS];
            for (column, raw) in row.columns()[..table.n_columns].iter().enumerate() {
                values[column] =
                    decode_foreign_field(storage, table, column, *raw, txid, arena, sequences)?;
            }
            let len = rowenc::encoded_len(&values[..table.n_columns]);
            let encoded = arena
                .alloc_slice_with(len, |_| 0u8)
                .map_err(|_| super::eval::arena_full())?;
            rowenc::encode(&values[..table.n_columns], encoded);
            visit(tuple_id, encoded)
        },
    )
}

/// The rows a remote command's tag reports (`INSERT 0 3`, `UPDATE 1`).
fn tag_rows(tag: &str) -> Result<u64, SqlError> {
    tag.rsplit(' ')
        .next()
        .and_then(|rows| rows.parse::<u64>().ok())
        .ok_or_else(|| {
            sql_err!(
                sqlstate::PROTOCOL_VIOLATION,
                "foreign command tag \"{}\" has no row count",
                tag
            )
        })
}

/// One local row for a remote INSERT. `retrieved` names the columns the
/// caller reads back (its `RETURNING` list, an `AFTER ROW` trigger's whole
/// row, a `WITH CHECK OPTION`); only then does the remote command return
/// them, and the other columns of `values` become NULL, as in postgres_fdw.
pub(crate) struct RemoteInsert<'values, 'data> {
    pub values: &'values mut [Datum<'data>],
    pub on_conflict_do_nothing: bool,
    pub retrieved: ColumnSet,
    pub render: crate::sql::guc::RenderContext,
    /// A `BEFORE ROW INSERT` trigger applies, which rules out batching.
    pub before_row_triggers: bool,
}

/// postgres_fdw's `deparseInsertSql` for `rows` rows: every column, a
/// generated one as `DEFAULT`, then the `RETURNING` list `retrieved` needs.
fn write_insert(
    output: &mut impl core::fmt::Write,
    remote: &RemoteTable,
    rows: usize,
    on_conflict_do_nothing: bool,
    retrieved: ColumnSet,
) {
    let table = &remote.table;
    let _ = output.write_str("INSERT INTO ");
    remote.write_relation(output);
    if table.n_columns == 0 {
        let _ = output.write_str(" DEFAULT VALUES");
    } else {
        let _ = output.write_char('(');
        for column in 0..table.n_columns {
            if column != 0 {
                let _ = output.write_str(", ");
            }
            remote.write_column(output, column);
        }
        let _ = output.write_str(") VALUES ");
        let mut parameter = 1;
        for row in 0..rows {
            if row != 0 {
                let _ = output.write_str(", ");
            }
            let _ = output.write_char('(');
            for (column, definition) in table.columns().iter().enumerate() {
                if column != 0 {
                    let _ = output.write_str(", ");
                }
                if definition.default.is_generated() {
                    let _ = output.write_str("DEFAULT");
                } else {
                    let _ = write!(output, "${parameter}");
                    parameter += 1;
                }
            }
            let _ = output.write_char(')');
        }
    }
    if on_conflict_do_nothing {
        let _ = output.write_str(" ON CONFLICT DO NOTHING");
    }
    let mut first = true;
    for column in 0..table.n_columns {
        if retrieved.contains(column) {
            let _ = output.write_str(if first { " RETURNING " } else { ", " });
            first = false;
            remote.write_column(output, column);
        }
    }
}

/// Inserts one fully evaluated local row, returning whether the remote
/// server stored it: its command tag's count, which is 0 when `ON CONFLICT
/// DO NOTHING` skipped it. Values cross as length-delimited Bind fields;
/// only catalog-derived identifiers enter the SQL source.
pub(crate) fn insert_row<'a>(
    storage: &Storage,
    table_slot: usize,
    txid: u32,
    request: RemoteInsert<'_, 'a>,
    arena: &'a Arena,
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<bool, SqlError> {
    let RemoteInsert {
        values,
        on_conflict_do_nothing,
        retrieved,
        render,
        before_row_triggers: _,
    } = request;
    let remote = RemoteTable::open(storage, table_slot, txid)?;
    let table = &remote.table;
    let mut query = StackStr::<16_384>::new();
    write_insert(&mut query, &remote, 1, on_conflict_do_nothing, retrieved);
    checked(&query, "INSERT query")?;
    let mut parameters = [None; crate::storage::MAX_COLUMNS];
    let mut count = 0;
    for (value, definition) in values.iter().zip(table.columns()) {
        if !definition.default.is_generated() {
            parameters[count] = Responder::datum_wire_text(value, render, arena)?;
            count += 1;
        }
    }
    let lease = enter_session(storage, txid, &remote.server)?;
    let fields = retrieved.count_ones() as usize;
    let mut returned = false;
    let mut inserted = None;
    exchange(
        storage,
        lease,
        query.as_str(),
        Some(&parameters[..count]),
        query.as_str(),
        &mut |event| match event {
            SqlEvent::RowDescription { fields: received } if received as usize != fields => {
                Err(sql_err!(
                    sqlstate::PROTOCOL_VIOLATION,
                    "foreign INSERT returned {} columns, expected {}",
                    received,
                    fields
                ))
            }
            SqlEvent::RowDescription { .. } => Ok(()),
            SqlEvent::DataRow(row) => {
                if returned || row.columns().len() != fields {
                    return Err(sql_err!(
                        sqlstate::PROTOCOL_VIOLATION,
                        "foreign INSERT returned an invalid row count or shape"
                    ));
                }
                returned = true;
                let mut raw = row.columns().iter();
                for (column, value) in values.iter_mut().enumerate() {
                    *value = if retrieved.contains(column) {
                        let field = raw.next().copied().flatten();
                        let field = match field {
                            Some(field) => Some(
                                &*arena
                                    .alloc_slice_copy(field)
                                    .map_err(|_| super::eval::arena_full())?,
                            ),
                            None => None,
                        };
                        decode_foreign_field(storage, table, column, field, txid, arena, sequences)?
                    } else {
                        Datum::Null
                    };
                }
                Ok(())
            }
            SqlEvent::CommandComplete { tag } if tag.starts_with("INSERT ") => {
                inserted = Some(tag_rows(tag)? != 0);
                Ok(())
            }
            _ => Err(unexpected_response("INSERT")),
        },
    )?;
    let inserted = inserted.ok_or_else(|| unexpected_response("INSERT"))?;
    if !retrieved.is_empty() && inserted != returned {
        return Err(sql_err!(
            sqlstate::PROTOCOL_VIOLATION,
            "foreign INSERT row count disagrees with its RETURNING rows"
        ));
    }
    Ok(inserted)
}

/// The most foreign tables one INSERT batches rows for at once.
const MAX_BATCH_TARGETS: usize = 16;

/// PostgreSQL's limit on one statement's parameters.
const MAX_QUERY_PARAMETERS: usize = 65_535;

/// A `fmt::Write` into a fixed arena buffer that records overflow.
struct FixedText<'b> {
    buffer: &'b mut [u8],
    len: usize,
    truncated: bool,
}

impl core::fmt::Write for FixedText<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let end = self.len + text.len();
        if end > self.buffer.len() {
            self.truncated = true;
            return Err(core::fmt::Error);
        }
        self.buffer[self.len..end].copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// Rows queued for one foreign table's next batched INSERT. Each value is
/// kept as its Bind field: a big-endian length (-1 for NULL), then bytes.
struct PendingInsert<'a> {
    table: usize,
    batch_size: usize,
    on_conflict_do_nothing: bool,
    rows: usize,
    buffer: &'a mut [u8],
    used: usize,
}

/// postgres_fdw's batched INSERT (`batch_size`): an INSERT statement queues
/// rows per foreign table and sends each table's rows as one multi-row
/// remote INSERT when the batch is full and the next row arrives, before a
/// `BEFORE ROW INSERT` trigger fires, and when the statement's rows end.
/// Each table's queue holds at most `foreign_send_bytes` of values, the most
/// one remote command can carry.
pub(crate) struct ForeignInsertBatches<'a> {
    pending: [Option<PendingInsert<'a>>; MAX_BATCH_TARGETS],
    /// Queued rows the remote server did not store (`ON CONFLICT DO
    /// NOTHING`), which the statement counted when queueing them.
    skipped: u64,
}

impl<'a> ForeignInsertBatches<'a> {
    pub(crate) fn new() -> Self {
        Self {
            pending: [const { None }; MAX_BATCH_TARGETS],
            skipped: 0,
        }
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.pending
            .iter()
            .any(|pending| pending.as_ref().is_some_and(|pending| pending.rows != 0))
    }

    /// Rows counted when queued that the remote server skipped.
    pub(crate) fn take_skipped(&mut self) -> u64 {
        core::mem::take(&mut self.skipped)
    }

    /// Queues one row, or inserts it at once when the table does not batch.
    /// Returns whether the row counts as inserted now: a queued row counts
    /// until its batch reports otherwise.
    pub(crate) fn insert_row(
        &mut self,
        storage: &Storage,
        table_slot: usize,
        txid: u32,
        request: RemoteInsert<'_, 'a>,
        arena: &'a Arena,
        sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
    ) -> Result<bool, SqlError> {
        let remote = RemoteTable::open(storage, table_slot, txid)?;
        let parameters = remote
            .table
            .columns()
            .iter()
            .filter(|column| !column.default.is_generated())
            .count();
        // postgres_fdw sends one row at a time when anything reads the
        // stored row back, a BEFORE ROW trigger could read earlier rows, or
        // the table has no columns.
        let batch_size =
            if request.retrieved.is_empty() && !request.before_row_triggers && parameters != 0 {
                usize::try_from(remote.batch_size())
                    .unwrap_or(1)
                    .min(MAX_QUERY_PARAMETERS / parameters)
            } else {
                1
            };
        if batch_size <= 1 {
            return insert_row(storage, table_slot, txid, request, arena, sequences);
        }
        let index = match self.pending.iter().position(|pending| {
            pending
                .as_ref()
                .is_some_and(|pending| pending.table == table_slot)
        }) {
            Some(index) => index,
            None => {
                let index = self
                    .pending
                    .iter()
                    .position(Option::is_none)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "an INSERT can batch rows for at most {} foreign tables",
                            MAX_BATCH_TARGETS
                        )
                    })?;
                let buffer = arena
                    .alloc_persistent_slice_with(storage.foreign_send_capacity(), |_| 0u8)
                    .map_err(|_| super::eval::arena_full())?;
                self.pending[index] = Some(PendingInsert {
                    table: table_slot,
                    batch_size,
                    on_conflict_do_nothing: request.on_conflict_do_nothing,
                    rows: 0,
                    buffer,
                    used: 0,
                });
                index
            }
        };
        if self.pending[index]
            .as_ref()
            .is_some_and(|pending| pending.rows == pending.batch_size)
        {
            self.flush(storage, txid, index, arena)?;
        }
        let pending = self.pending[index]
            .as_mut()
            .expect("the batch target was found or created");
        let mark = pending.used;
        for (value, column) in request.values.iter().zip(remote.table.columns()) {
            if column.default.is_generated() {
                continue;
            }
            let text = Responder::datum_wire_text(value, request.render, arena)?;
            let length = text.map_or(-1, |text| text.len() as i32);
            let end = pending.used + 4 + text.map_or(0, <[u8]>::len);
            if end > pending.buffer.len() {
                pending.used = mark;
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "foreign INSERT batch for table \"{}\" exceeds foreign_send_bytes ({})",
                    remote.table.name.as_str(),
                    pending.buffer.len()
                ));
            }
            pending.buffer[pending.used..pending.used + 4].copy_from_slice(&length.to_be_bytes());
            if let Some(text) = text {
                pending.buffer[pending.used + 4..end].copy_from_slice(text);
            }
            pending.used = end;
        }
        pending.rows += 1;
        Ok(true)
    }

    /// Sends every queued row.
    pub(crate) fn flush_all(
        &mut self,
        storage: &Storage,
        txid: u32,
        arena: &Arena,
    ) -> Result<(), SqlError> {
        for index in 0..MAX_BATCH_TARGETS {
            if self.pending[index]
                .as_ref()
                .is_some_and(|pending| pending.rows != 0)
            {
                self.flush(storage, txid, index, arena)?;
            }
        }
        Ok(())
    }

    /// postgres_fdw's `execute_foreign_modify` for a batch: one INSERT with
    /// a VALUES row per queued row, counted by its command tag.
    fn flush(
        &mut self,
        storage: &Storage,
        txid: u32,
        index: usize,
        arena: &Arena,
    ) -> Result<(), SqlError> {
        let pending = self.pending[index]
            .as_mut()
            .expect("a flushed batch target exists");
        let rows = core::mem::take(&mut pending.rows);
        let used = core::mem::take(&mut pending.used);
        let remote = RemoteTable::open(storage, pending.table, txid)?;
        let per_row = remote
            .table
            .columns()
            .iter()
            .filter(|column| !column.default.is_generated())
            .count();
        let count = rows * per_row;
        let parameters = arena
            .alloc_slice_with(count, |_| None::<&[u8]>)
            .map_err(|_| super::eval::arena_full())?;
        let mut at = 0;
        for parameter in parameters.iter_mut() {
            let length = i32::from_be_bytes(
                pending.buffer[at..at + 4]
                    .try_into()
                    .expect("a queued value has its length"),
            );
            at += 4;
            if length >= 0 {
                let end = at + length as usize;
                *parameter = Some(&pending.buffer[at..end]);
                at = end;
            }
        }
        debug_assert_eq!(at, used);
        let mut single = StackStr::<16_384>::new();
        write_insert(
            &mut single,
            &remote,
            1,
            pending.on_conflict_do_nothing,
            ColumnSet::EMPTY,
        );
        checked(&single, "INSERT query")?;
        let capacity =
            single.as_str().len() + rows * (per_row * 9 + remote.table.n_columns * 9 + 4);
        let mut query = FixedText {
            buffer: arena
                .alloc_slice_with(capacity, |_| 0u8)
                .map_err(|_| super::eval::arena_full())?,
            len: 0,
            truncated: false,
        };
        write_insert(
            &mut query,
            &remote,
            rows,
            pending.on_conflict_do_nothing,
            ColumnSet::EMPTY,
        );
        if query.truncated {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "foreign batched INSERT query exceeds its buffer"
            ));
        }
        let query = core::str::from_utf8(&query.buffer[..query.len])
            .expect("the INSERT text is written from strings");
        let lease = enter_session(storage, txid, &remote.server)?;
        let mut inserted = None;
        exchange(
            storage,
            lease,
            query,
            Some(&*parameters),
            query,
            &mut |event| match event {
                SqlEvent::CommandComplete { tag } if tag.starts_with("INSERT ") => {
                    inserted = Some(tag_rows(tag)?);
                    Ok(())
                }
                _ => Err(unexpected_response("INSERT")),
            },
        )?;
        let inserted = inserted.ok_or_else(|| unexpected_response("INSERT"))?;
        self.skipped += (rows as u64).saturating_sub(inserted);
        Ok(())
    }
}

/// Runs a remote UPDATE, DELETE or row fetch that returns at most one whole
/// row, encoded for the local executor.
#[expect(
    clippy::too_many_arguments,
    reason = "foreign values preserve their explicit statement sequence context"
)]
fn remote_returning_row<'a>(
    storage: &Storage,
    lease: ForeignSessionLease,
    table: &TableDef,
    arena: &'a Arena,
    query: &str,
    parameters: &[Option<&[u8]>],
    operation: &str,
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<Option<&'a [u8]>, SqlError> {
    let txid = lease.transaction_id();
    let mut returned = None;
    exchange(
        storage,
        lease,
        query,
        Some(parameters),
        query,
        &mut |event| match event {
            SqlEvent::RowDescription { fields } if fields as usize != table.n_columns => {
                Err(sql_err!(
                    sqlstate::PROTOCOL_VIOLATION,
                    "foreign {} returned {} columns, expected {}",
                    operation,
                    fields,
                    table.n_columns
                ))
            }
            SqlEvent::RowDescription { .. } | SqlEvent::CommandComplete { .. } => Ok(()),
            SqlEvent::DataRow(row) => {
                if returned.is_some() || row.columns().len() != table.n_columns {
                    return Err(sql_err!(
                        sqlstate::PROTOCOL_VIOLATION,
                        "foreign {} returned an invalid row count or shape",
                        operation
                    ));
                }
                let mut values = [Datum::Null; crate::storage::MAX_COLUMNS];
                for (column, raw) in row.columns().iter().enumerate() {
                    values[column] =
                        decode_foreign_field(storage, table, column, *raw, txid, arena, sequences)?;
                }
                let len = rowenc::encoded_len(&values[..table.n_columns]);
                let encoded = arena
                    .alloc_slice_with(len, |_| 0u8)
                    .map_err(|_| super::eval::arena_full())?;
                rowenc::encode(&values[..table.n_columns], encoded);
                returned = Some(&*encoded);
                Ok(())
            }
            _ => Err(sql_err!(
                sqlstate::PROTOCOL_VIOLATION,
                "unexpected foreign PostgreSQL {} response",
                operation
            )),
        },
    )?;
    Ok(returned)
}

fn write_column_list<const N: usize>(output: &mut StackStr<N>, remote: &RemoteTable) {
    for column in 0..remote.table.n_columns {
        if column != 0 {
            let _ = output.write_str(", ");
        }
        remote.write_column(output, column);
    }
}

/// Reads the current remote image identified by a parsed `ctid`.  Callers use
/// this after selection and before local trigger/evaluation work, so remote
/// tuple identity remains opaque to SQL expressions and local heap storage.
pub(crate) fn fetch_row<'a>(
    storage: &Storage,
    table_slot: usize,
    txid: u32,
    tuple_id: RemoteTupleId,
    arena: &'a Arena,
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<Option<&'a [u8]>, SqlError> {
    let remote = RemoteTable::open(storage, table_slot, txid)?;
    let mut query = StackStr::<16_384>::new();
    let _ = query.write_str("SELECT ");
    write_column_list(&mut query, &remote);
    let _ = query.write_str(" FROM ");
    remote.write_relation(&mut query);
    let _ = query.write_str(" WHERE ctid = $1::tid");
    checked(&query, "row fetch query")?;
    let mut parameter = StackStr::<32>::new();
    tuple_id.write_text(&mut parameter);
    let lease = enter_session(storage, txid, &remote.server)?;
    remote_returning_row(
        storage,
        lease,
        &remote.table,
        arena,
        query.as_str(),
        &[Some(parameter.as_str().as_bytes())],
        "row fetch",
        sequences,
    )
}

/// The fully parsed local image for one remote `UPDATE`.  The locator and
/// changed-column list are distinct typed inputs, so no caller can smuggle a
/// SQL fragment or claim that every column changed.
pub(crate) struct RemoteUpdate<'values, 'data> {
    pub tuple_id: RemoteTupleId,
    pub target_columns: &'values [usize],
    pub values: &'values mut [Datum<'data>],
    pub render: crate::sql::guc::RenderContext,
}

/// Updates exactly the parsed remote tuple and replaces `values` with its
/// `RETURNING` image.  The tuple locator is a typed text Bind field, never
/// interpolated into SQL source.
pub(crate) fn update_row<'data>(
    storage: &Storage,
    table_slot: usize,
    txid: u32,
    request: RemoteUpdate<'_, 'data>,
    arena: &'data Arena,
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<bool, SqlError> {
    let RemoteUpdate {
        tuple_id,
        target_columns,
        values,
        render,
    } = request;
    let remote = RemoteTable::open(storage, table_slot, txid)?;
    let mut query = StackStr::<16_384>::new();
    let _ = query.write_str("UPDATE ");
    remote.write_relation(&mut query);
    let _ = query.write_str(" SET ");
    for (parameter, &column) in target_columns.iter().enumerate() {
        if parameter != 0 {
            let _ = query.write_str(", ");
        }
        remote.write_column(&mut query, column);
        let _ = write!(query, " = ${}", parameter + 1);
    }
    let _ = write!(
        query,
        " WHERE ctid = ${}::tid RETURNING ",
        target_columns.len() + 1
    );
    write_column_list(&mut query, &remote);
    checked(&query, "UPDATE query")?;
    let mut parameters = [None; crate::storage::MAX_COLUMNS + 1];
    for (parameter, &column) in target_columns.iter().enumerate() {
        parameters[parameter] = Responder::datum_wire_text(&values[column], render, arena)?;
    }
    let mut identity = StackStr::<32>::new();
    tuple_id.write_text(&mut identity);
    parameters[target_columns.len()] = Some(identity.as_str().as_bytes());
    let lease = enter_session(storage, txid, &remote.server)?;
    let Some(row) = remote_returning_row(
        storage,
        lease,
        &remote.table,
        arena,
        query.as_str(),
        &parameters[..target_columns.len() + 1],
        "UPDATE",
        sequences,
    )?
    else {
        return Ok(false);
    };
    let mut schema = [crate::sql::types::ColType::Bool; crate::storage::MAX_COLUMNS];
    remote.table.schema(&mut schema);
    rowenc::decode(row, &schema[..remote.table.n_columns], values)?;
    Ok(true)
}

/// Deletes exactly the parsed remote tuple and returns its old image when the
/// tuple was still visible in the transaction snapshot.
pub(crate) fn delete_row<'a>(
    storage: &Storage,
    table_slot: usize,
    txid: u32,
    tuple_id: RemoteTupleId,
    arena: &'a Arena,
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<Option<&'a [u8]>, SqlError> {
    let remote = RemoteTable::open(storage, table_slot, txid)?;
    let mut query = StackStr::<16_384>::new();
    let _ = query.write_str("DELETE FROM ");
    remote.write_relation(&mut query);
    let _ = query.write_str(" WHERE ctid = $1::tid RETURNING ");
    write_column_list(&mut query, &remote);
    checked(&query, "DELETE query")?;
    let mut parameter = StackStr::<32>::new();
    tuple_id.write_text(&mut parameter);
    let lease = enter_session(storage, txid, &remote.server)?;
    remote_returning_row(
        storage,
        lease,
        &remote.table,
        arena,
        query.as_str(),
        &[Some(parameter.as_str().as_bytes())],
        "DELETE",
        sequences,
    )
}

pub(crate) fn truncate_tables(
    storage: &Storage,
    table_slots: &[usize],
    txid: u32,
    restart_identity: bool,
    cascade: bool,
) -> Result<(), SqlError> {
    let Some(&first) = table_slots.first() else {
        return Ok(());
    };
    let server = RemoteTable::open(storage, first, txid)?.server;
    let mut query = StackStr::<16_384>::new();
    let _ = query.write_str("TRUNCATE TABLE ");
    for (index, slot) in table_slots.iter().enumerate() {
        let remote = RemoteTable::open(storage, *slot, txid)?;
        if remote.server.key != server.key {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "TRUNCATE of foreign tables on different servers is not supported"
            ));
        }
        if index != 0 {
            let _ = query.write_str(", ");
        }
        remote.write_relation(&mut query);
    }
    let _ = query.write_str(if restart_identity {
        " RESTART IDENTITY"
    } else {
        " CONTINUE IDENTITY"
    });
    let _ = query.write_str(if cascade { " CASCADE" } else { " RESTRICT" });
    checked(&query, "TRUNCATE query")?;
    let lease = enter_session(storage, txid, &server)?;
    command(storage, lease, query.as_str(), "TRUNCATE TABLE")?;
    Ok(())
}

/// One `postgres_fdw_get_connections` row source: the 1.1 form returns
/// `server_name` and `valid`; 1.2 adds `user_name`, `used_in_xact`, `closed`
/// and `remote_backend_pid`.
pub(crate) fn connection_rows<'a>(
    storage: &Storage,
    txid: u32,
    version_1_2: bool,
    check_connections: bool,
    arena: &'a Arena,
) -> Result<&'a [&'a [u8]], SqlError> {
    const EMPTY: crate::storage::ForeignConnectionStatus =
        crate::storage::ForeignConnectionStatus {
            server: 0,
            server_created_at: 0,
            mapping: 0,
            mapping_created_at: 0,
            valid: false,
            used_in_transaction: false,
            remote_backend_pid: 0,
            raw_fd: None,
        };
    let mut statuses = [EMPTY; MAX_LISTED_CONNECTIONS];
    let count = storage.foreign_connections(storage.current_connection_id(), &mut statuses);
    const NO_ROW: &[u8] = &[];
    let rows = arena
        .alloc_slice_with(count, |_| NO_ROW)
        .map_err(|_| super::eval::arena_full())?;
    for (row, status) in rows.iter_mut().zip(&statuses[..count]) {
        let server = storage
            .foreign_server_by_slot(status.server as usize, txid)
            .filter(|_| {
                storage
                    .foreign_server_entry(status.server as usize)
                    .created_at
                    == status.server_created_at
            });
        let server_name = match server {
            Some(server) => Datum::Text(
                arena
                    .alloc_str(server.name.as_str())
                    .map_err(|_| super::eval::arena_full())?,
            ),
            None => Datum::Null,
        };
        let mut values = [Datum::Null; 6];
        if !version_1_2 {
            values[0] = server_name;
            values[1] = Datum::Bool(status.valid);
            *row = super::exec::encode_projected_pub(&values[..2], arena)?;
            continue;
        }
        let mapping = (storage
            .foreign_mapping_entry(status.mapping as usize)
            .created_at
            == status.mapping_created_at)
            .then(|| storage.foreign_mapping_by_slot(status.mapping as usize, txid))
            .flatten();
        values[0] = server_name;
        values[1] = match mapping.map(|mapping| mapping.user) {
            Some(ForeignMappingUser::Public) => Datum::Text("public"),
            Some(ForeignMappingUser::Role(role)) => Datum::Text(
                arena
                    .alloc_str(storage.role_name(role as usize, txid).as_str())
                    .map_err(|_| super::eval::arena_full())?,
            ),
            None => Datum::Null,
        };
        values[2] = Datum::Bool(status.valid);
        values[3] = Datum::Bool(status.used_in_transaction);
        values[4] = if check_connections {
            connection_closed(status.raw_fd).map_or(Datum::Null, Datum::Bool)
        } else {
            Datum::Null
        };
        values[5] = Datum::Int4(status.remote_backend_pid);
        *row = super::exec::encode_projected_pub(&values, arena)?;
    }
    Ok(rows)
}

/// The most cached connections one backend lists; `max_foreign_sessions`
/// bounds every backend's cache together.
const MAX_LISTED_CONNECTIONS: usize = 256;

/// postgres_fdw's `pgfdw_conn_check`, which PostgreSQL supports only where
/// `poll` reports `POLLRDHUP`; elsewhere the column is NULL.
fn connection_closed(raw_fd: Option<std::os::fd::RawFd>) -> Option<bool> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let Some(fd) = raw_fd else {
            return Some(true);
        };
        let mut descriptor = libc::pollfd {
            fd,
            events: libc::POLLRDHUP,
            revents: 0,
        };
        loop {
            let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
            if result < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            }
            if result < 0 {
                return Some(true);
            }
            return Some(
                descriptor.revents
                    & (libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)
                    != 0,
            );
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let _ = raw_fd;
        None
    }
}

/// `postgres_fdw_disconnect(server)` and, with `None`,
/// `postgres_fdw_disconnect_all()`: closes this backend's cached connections
/// that no transaction uses, warning about the others.
pub(crate) fn disconnect(
    storage: &Storage,
    txid: u32,
    server_name: Option<&str>,
) -> Result<bool, SqlError> {
    let server = match server_name {
        Some(name) => {
            let Some((slot, _)) = storage.foreign_server(name, txid) else {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "server \"{}\" does not exist",
                    name
                ));
            };
            Some((slot as u16, storage.foreign_server_entry(slot).created_at))
        }
        None => None,
    };
    let mut busy = [(0u16, 0u64); MAX_LISTED_CONNECTIONS];
    let (closed, busy_count) =
        storage.disconnect_foreign_connections(storage.current_connection_id(), server, &mut busy);
    for &(slot, created_at) in &busy[..busy_count] {
        let name = storage
            .foreign_server_by_slot(slot as usize, txid)
            .filter(|_| storage.foreign_server_entry(slot as usize).created_at == created_at);
        match name {
            Some(server) => super::eval::raise_eval_message(
                super::eval::EvalMessageLevel::Warning,
                sqlstate::WARNING,
                format_args!(
                    "cannot close connection for server \"{}\" because it is still in use",
                    server.name.as_str()
                ),
            )?,
            None => super::eval::raise_eval_message(
                super::eval::EvalMessageLevel::Warning,
                sqlstate::WARNING,
                format_args!("cannot close dropped server connection because it is still in use"),
            )?,
        }
    }
    Ok(closed)
}

/// A scalar call of one of postgres_fdw's SQL-callable routines.
pub(crate) fn call_native<'a>(
    storage: &Storage,
    txid: u32,
    native: crate::storage::NativeRoutine,
    arguments: &[Datum<'a>],
) -> Result<Datum<'a>, SqlError> {
    use crate::storage::NativeRoutine;
    match native {
        NativeRoutine::PostgresFdwDisconnect => {
            let name = super::eval::text_view(arguments[0]);
            let Datum::Text(name) = name else {
                return Err(sql_err!(
                    sqlstate::DATATYPE_MISMATCH,
                    "postgres_fdw_disconnect requires a text argument"
                ));
            };
            disconnect(storage, txid, Some(name)).map(Datum::Bool)
        }
        NativeRoutine::PostgresFdwDisconnectAll => disconnect(storage, txid, None).map(Datum::Bool),
        NativeRoutine::PostgresFdwValidator => {
            super::exec::call_postgres_fdw_validator(arguments[0], arguments[1])?;
            Ok(Datum::Null)
        }
        NativeRoutine::PostgresFdwGetConnections
        | NativeRoutine::PostgresFdwGetConnections12
        | NativeRoutine::PostgresFdwHandler => Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "set-valued function called in context that cannot accept a set"
        )),
    }
}

/// The rows of `postgres_fdw_get_connections`.
pub(crate) fn native_rows<'a>(
    storage: &Storage,
    txid: u32,
    native: crate::storage::NativeRoutine,
    arguments: &[Datum<'a>],
    arena: &'a Arena,
) -> Result<&'a [&'a [u8]], SqlError> {
    use crate::storage::NativeRoutine;
    match native {
        NativeRoutine::PostgresFdwGetConnections => {
            connection_rows(storage, txid, false, false, arena)
        }
        NativeRoutine::PostgresFdwGetConnections12 => {
            let check = matches!(arguments.first(), Some(Datum::Bool(true)));
            connection_rows(storage, txid, true, check, arena)
        }
        _ => Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "native routine is not set-returning"
        )),
    }
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::RemoteTupleId;
    use crate::util::StackStr;

    #[test]
    fn remote_tuple_identity_is_parsed_once_and_rendered_canonically() {
        let tuple = RemoteTupleId::parse(b"(4294967295,65535)").unwrap();
        let mut rendered = StackStr::<32>::new();
        tuple.write_text(&mut rendered);
        assert_eq!(rendered.as_str(), "(4294967295,65535)");
        assert_eq!(tuple.sort_key(), (1u64 << 48) - 1);
    }

    #[test]
    fn remote_tuple_identity_rejects_untyped_or_impossible_values() {
        for value in [
            b"".as_slice(),
            b"1,2",
            b"(1)",
            b"(1,0)",
            b"(1,-1)",
            b"(4294967296,1)",
            b"(1,65536)",
            b"(1,2,3)",
        ] {
            assert!(RemoteTupleId::parse(value).is_err(), "{value:?}");
        }
    }
}
