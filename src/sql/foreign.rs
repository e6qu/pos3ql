//! Bounded postgres_fdw transport and row materialization.

use core::fmt::Write as _;
use std::time::{Duration, Instant};

use crate::mem::arena::Arena;
use crate::pg::replication_client::{ClientError, ClientEvent, ConnectionInfo, SqlEvent, SslMode};
use crate::pg::respond::Responder;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::types::Datum;
use crate::sql_err;
use crate::storage::foreign::{ForeignDataHandler, ForeignMappingUser, ForeignServerDefinition};
use crate::storage::{PrivilegeSet, Storage, TableDef, rowenc};
use crate::util::StackStr;

const DEFAULT_CONNECT_TIMEOUT_SECONDS: u64 = 10;

/// A PostgreSQL heap tuple identity returned by `ctid::text`.  Parsing it at
/// the transport boundary prevents a remote row locator from being confused
/// with user SQL or an arbitrary parameter string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RemoteTupleId {
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

    pub fn write_text<const N: usize>(self, output: &mut StackStr<N>) {
        let _ = write!(output, "({},{})", self.block, self.offset);
    }

    pub(crate) fn sort_key(self) -> u64 {
        (u64::from(self.block) << 16) | u64::from(self.offset)
    }
}

fn client_error(error: ClientError) -> SqlError {
    match error {
        ClientError::Publisher(diagnostic) => SqlError {
            sqlstate: diagnostic.sqlstate,
            message: diagnostic.message,
        },
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
fn decode_foreign_field<'a>(
    storage: &Storage,
    table: &TableDef,
    column: usize,
    raw: Option<&'a [u8]>,
    txid: u32,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let declared = storage.declared_column_type(&table.columns()[column], txid)?;
    let value = match raw {
        Some(raw) => {
            super::exec::decode_text_input(storage, declared.catalog_oid(), raw, arena, txid)?
        }
        None => {
            super::exec::coerce_binary_input_null(storage, declared.catalog_oid(), arena, txid)?
        }
    };
    if value.is_null() {
        return Ok(value);
    }
    super::exec::coerce(value, &table.columns()[column], storage, txid, arena)
}

fn poll_client(
    client: &mut crate::pg::replication_client::ReplicationClient,
    deadline: Instant,
    visit: &mut impl FnMut(ClientEvent<'_>) -> Result<(), ClientError>,
) -> Result<(), SqlError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(sql_err!(
            sqlstate::QUERY_CANCELED,
            "foreign PostgreSQL operation timed out"
        ));
    }
    let timeout = remaining.as_millis().min(i32::MAX as u128) as i32;
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
            return Err(sql_err!(
                sqlstate::QUERY_CANCELED,
                "foreign PostgreSQL operation timed out"
            ));
        }
        break;
    }
    if descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
        return Err(sql_err!(
            sqlstate::FDW_ERROR,
            "foreign PostgreSQL transport closed"
        ));
    }
    if descriptor.revents & libc::POLLOUT != 0 {
        client.writable().map_err(client_error)?;
    }
    if descriptor.revents & libc::POLLIN != 0 {
        client.readable(visit).map_err(client_error)?;
    }
    Ok(())
}

/// Enters the remote transaction corresponding to `txid`.  The transport has
/// one startup-reserved session, and Storage records its typed owner before any
/// later foreign operation can reuse it.
fn enter_session(
    storage: &Storage,
    txid: u32,
    endpoint: ConnectionInfo,
    timeout: Duration,
) -> Result<(), SqlError> {
    if storage.foreign_session_active(txid, endpoint)? {
        return Ok(());
    }
    let mut client = storage.foreign_client()?;
    let result = (|| {
        client.bind_sql(endpoint).map_err(client_error)?;
        let deadline = Instant::now() + timeout;
        let mut ready = false;
        while !ready {
            poll_client(&mut client, deadline, &mut |event| {
                match event {
                    ClientEvent::Sql(SqlEvent::Ready {
                        transaction_status: b'I',
                    }) => ready = true,
                    _ => {
                        return Err(ClientError::Protocol(
                            crate::pg::replication_client::FrameError::Malformed,
                        ));
                    }
                }
                Ok(())
            })?;
        }
        // postgres_fdw mirrors SERIALIZABLE and otherwise uses REPEATABLE
        // READ, preserving one remote snapshot across local statements.
        let begin = if storage.foreign_statement_is_serializable(txid) {
            "BEGIN ISOLATION LEVEL SERIALIZABLE"
        } else {
            "BEGIN ISOLATION LEVEL REPEATABLE READ"
        };
        client.query(begin).map_err(client_error)?;
        ready = false;
        while !ready {
            poll_client(&mut client, deadline, &mut |event| {
                match event {
                    ClientEvent::Sql(SqlEvent::Ready {
                        transaction_status: b'T',
                    }) => ready = true,
                    ClientEvent::Sql(SqlEvent::CommandComplete { tag: "BEGIN" }) => {}
                    _ => {
                        return Err(ClientError::Protocol(
                            crate::pg::replication_client::FrameError::Malformed,
                        ));
                    }
                }
                Ok(())
            })?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        client.unbind();
        return Err(error);
    }
    drop(client);
    storage.activate_foreign_session(txid, endpoint, timeout);
    Ok(())
}

fn session_command(
    storage: &Storage,
    txid: u32,
    command: &str,
    completion: &str,
    transaction_status: u8,
) -> Result<(), SqlError> {
    let Some(timeout) = storage.foreign_session_timeout(txid) else {
        return Ok(());
    };
    let mut client = storage.foreign_client()?;
    (|| {
        client.query(command).map_err(client_error)?;
        let deadline = Instant::now() + timeout;
        let mut ready = false;
        while !ready {
            poll_client(&mut client, deadline, &mut |event| {
                match event {
                    ClientEvent::Sql(SqlEvent::CommandComplete { tag }) if tag == completion => {}
                    ClientEvent::Sql(SqlEvent::Ready {
                        transaction_status: status,
                    }) if status == transaction_status => ready = true,
                    _ => {
                        return Err(ClientError::Protocol(
                            crate::pg::replication_client::FrameError::Malformed,
                        ));
                    }
                }
                Ok(())
            })?;
        }
        Ok(())
    })()
}

fn close_session(storage: &Storage, txid: u32, command: &str) -> Result<(), SqlError> {
    let result = session_command(storage, txid, command, command, b'I');
    if storage.foreign_session_endpoint(txid).is_none() {
        return result;
    }
    let mut client = storage.foreign_client()?;
    client.unbind();
    drop(client);
    storage.clear_foreign_session(txid);
    result
}

pub(crate) fn commit_session(storage: &Storage, txid: u32) -> Result<(), SqlError> {
    close_session(storage, txid, "COMMIT")
}

/// ROLLBACK has no error channel in PostgreSQL's command protocol.  If the
/// wire exchange cannot complete, dropping its sole transport forces the
/// remote backend to abort the transaction before the session slot is reused.
pub(crate) fn abort_session(storage: &Storage, txid: u32) {
    let _ = close_session(storage, txid, "ROLLBACK");
}

fn savepoint_command(
    storage: &Storage,
    txid: u32,
    prefix: &str,
    name: &str,
) -> Result<(), SqlError> {
    if storage.foreign_session_endpoint(txid).is_none() {
        return Ok(());
    }
    let mut command = StackStr::<256>::new();
    let _ = command.write_str(prefix);
    quote_identifier(&mut command, name);
    if command.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "foreign savepoint command exceeds its fixed buffer"
        ));
    }
    let completion = match prefix {
        "SAVEPOINT " => "SAVEPOINT",
        "RELEASE SAVEPOINT " => "RELEASE",
        "ROLLBACK TO SAVEPOINT " => "ROLLBACK",
        _ => unreachable!("foreign savepoint command has a fixed prefix"),
    };
    session_command(storage, txid, command.as_str(), completion, b'T')
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

fn quote_identifier<const N: usize>(output: &mut StackStr<N>, value: &str) {
    let _ = output.write_char('"');
    for character in value.chars() {
        if character == '"' {
            let _ = output.write_char('"');
        }
        let _ = output.write_char(character);
    }
    let _ = output.write_char('"');
}

fn quote_remote_column<const N: usize>(
    output: &mut StackStr<N>,
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
    let (endpoint, _, timeout) = connection_for_server(storage, server_slot, txid)?;
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

    const EMPTY: ImportCommand<'static> = ImportCommand { sql: "" };
    let mut commands: *mut ImportCommand<'a> = core::ptr::null_mut();
    let mut count = 0usize;
    let mut capacity = 0usize;
    let mut response_error = None;
    enter_session(storage, txid, endpoint, timeout)?;
    let mut ready = false;
    let mut client = storage.foreign_client()?;
    let execution = (|| -> Result<(), SqlError> {
        let deadline = Instant::now() + timeout;
        client.query(query.as_str()).map_err(client_error)?;
        ready = false;
        while !ready && response_error.is_none() {
            poll_client(&mut client, deadline, &mut |event| {
                match event {
                    ClientEvent::Sql(SqlEvent::RowDescription { fields }) if fields != 3 => {
                        response_error = Some(sql_err!(
                            sqlstate::PROTOCOL_VIOLATION,
                            "foreign import returned {} columns, expected 3",
                            fields
                        ));
                    }
                    ClientEvent::Sql(SqlEvent::DataRow(row)) => {
                        let columns = row.columns();
                        let [Some(name), Some(column_sql), Some(partition)] = columns else {
                            response_error = Some(sql_err!(
                                sqlstate::PROTOCOL_VIOLATION,
                                "foreign import returned an invalid catalog row"
                            ));
                            return Ok(());
                        };
                        let partition = match *partition {
                            b"t" | b"true" => true,
                            b"f" | b"false" => false,
                            _ => {
                                response_error = Some(sql_err!(
                                    sqlstate::PROTOCOL_VIOLATION,
                                    "foreign import returned an invalid partition flag"
                                ));
                                return Ok(());
                            }
                        };
                        let name = match core::str::from_utf8(name) {
                            Ok(name) => name,
                            Err(_) => {
                                response_error = Some(sql_err!(
                                    sqlstate::CHARACTER_NOT_IN_REPERTOIRE,
                                    "foreign import returned a non-UTF8 relation name"
                                ));
                                return Ok(());
                            }
                        };
                        let column_sql = match core::str::from_utf8(column_sql) {
                            Ok(sql) => sql,
                            Err(_) => {
                                response_error = Some(sql_err!(
                                    sqlstate::CHARACTER_NOT_IN_REPERTOIRE,
                                    "foreign import returned a non-UTF8 table definition"
                                ));
                                return Ok(());
                            }
                        };
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
                        if sql.is_truncated() {
                            response_error = Some(sql_err!(
                                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                "imported foreign-table definition exceeds its fixed buffer"
                            ));
                            return Ok(());
                        }
                        let sql = match arena.alloc_str(sql.as_str()) {
                            Ok(sql) => sql,
                            Err(_) => {
                                response_error = Some(super::eval::arena_full());
                                return Ok(());
                            }
                        };
                        if count == capacity {
                            let next = if capacity == 0 { 8 } else { capacity * 2 };
                            let fresh = match arena.alloc_slice_with(next, |_| EMPTY) {
                                Ok(fresh) => fresh,
                                Err(_) => {
                                    response_error = Some(super::eval::arena_full());
                                    return Ok(());
                                }
                            };
                            if count != 0 {
                                let prior = unsafe { core::slice::from_raw_parts(commands, count) };
                                fresh[..count].copy_from_slice(prior);
                            }
                            commands = fresh.as_mut_ptr();
                            capacity = next;
                        }
                        unsafe { commands.add(count).write(ImportCommand { sql }) };
                        count += 1;
                    }
                    ClientEvent::Sql(SqlEvent::Ready { .. }) => ready = true,
                    ClientEvent::Sql(
                        SqlEvent::CommandComplete { .. } | SqlEvent::RowDescription { .. },
                    ) => {}
                    _ => {
                        response_error = Some(sql_err!(
                            sqlstate::PROTOCOL_VIOLATION,
                            "unexpected foreign PostgreSQL import response"
                        ));
                    }
                }
                Ok(())
            })?;
        }
        Ok(())
    })();
    drop(client);
    if let Err(error) = execution {
        abort_session(storage, txid);
        return Err(error);
    }
    if let Some(error) = response_error {
        return Err(error);
    }
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
) -> Result<(ConnectionInfo, ForeignServerDefinition, Duration), SqlError> {
    let server = storage
        .foreign_server_by_slot(server_slot, txid)
        .ok_or_else(|| sql_err!(sqlstate::UNDEFINED_OBJECT, "foreign server does not exist"))?;
    let wrapper = storage
        .foreign_wrapper_by_slot(server.wrapper as usize, txid)
        .ok_or_else(|| sql_err!(sqlstate::FDW_ERROR, "foreign-data wrapper does not exist"))?;
    if wrapper.handler != ForeignDataHandler::Postgres {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "foreign-data wrapper has no executable PostgreSQL handler"
        ));
    }
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
    let mapping = storage
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
        })?
        .1;

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
        .unwrap_or("5432")
        .parse::<u16>()
        .map_err(|_| sql_err!(sqlstate::FDW_ERROR, "invalid foreign server port"))?;
    let database = match server.options.get("dbname") {
        Some(name) => crate::storage::SqlName::parse(name).map_err(|_| {
            sql_err!(
                sqlstate::FDW_INVALID_ATTRIBUTE_VALUE,
                "invalid foreign database name"
            )
        })?,
        None => storage.current_database_name(txid),
    };
    let local_role = storage.role_name(role, txid);
    let user = mapping.options.get("user").unwrap_or(local_role.as_str());
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
    Ok((endpoint, server, Duration::from_secs(timeout)))
}

fn endpoint(
    storage: &Storage,
    table_slot: usize,
    txid: u32,
) -> Result<
    (
        ConnectionInfo,
        TableDef,
        crate::storage::foreign::ForeignTableDefinition,
        Duration,
    ),
    SqlError,
> {
    let table = *storage.table_def(table_slot, txid);
    let foreign = storage
        .foreign_table(table_slot as u16, txid)
        .ok_or_else(|| sql_err!(sqlstate::FDW_ERROR, "foreign table has no server binding"))?
        .1;
    let (endpoint, _, timeout) = connection_for_server(storage, foreign.server as usize, txid)?;
    Ok((endpoint, table, foreign, timeout))
}

pub(crate) fn materialize<'a>(
    storage: &'a Storage,
    table_slot: usize,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a [&'a [u8]], SqlError> {
    let (endpoint, table, foreign, timeout) = endpoint(storage, table_slot, txid)?;
    let remote_schema = foreign
        .options
        .get("schema_name")
        .unwrap_or(table.schema.as_str());
    let remote_table = foreign
        .options
        .get("table_name")
        .unwrap_or(table.name.as_str());
    let mut query = StackStr::<16_384>::new();
    let _ = query.write_str("SELECT ");
    for (column, definition) in table.columns().iter().enumerate() {
        if column != 0 {
            let _ = query.write_str(", ");
        }
        let remote = foreign
            .column_options
            .options_for(column as u16)
            .find(|option| option.name.as_str() == "column_name");
        match remote {
            Some(option) => quote_identifier(&mut query, option.value.as_str()),
            None => quote_identifier(&mut query, definition.name.as_str()),
        }
    }
    let _ = query.write_str(" FROM ");
    quote_identifier(&mut query, remote_schema);
    let _ = query.write_char('.');
    quote_identifier(&mut query, remote_table);
    if query.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "foreign query exceeds its fixed buffer"
        ));
    }

    const EMPTY: &[u8] = &[];
    let mut rows: *mut &[u8] = core::ptr::null_mut();
    let mut row_count = 0usize;
    let mut row_capacity = 0usize;
    let mut conversion_error = None;
    enter_session(storage, txid, endpoint, timeout)?;
    let mut ready = false;
    let mut client = storage.foreign_client()?;
    let execution = (|| -> Result<(), SqlError> {
        let deadline = Instant::now() + timeout;
        client.query(query.as_str()).map_err(client_error)?;
        ready = false;
        while !ready && conversion_error.is_none() {
            poll_client(&mut client, deadline, &mut |event| {
                match event {
                    ClientEvent::Sql(SqlEvent::RowDescription { fields })
                        if fields as usize != table.n_columns =>
                    {
                        conversion_error = Some(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "foreign row has {} columns, expected {}",
                            fields,
                            table.n_columns
                        ));
                    }
                    ClientEvent::Sql(SqlEvent::DataRow(remote)) => {
                        if remote.columns().len() != table.n_columns {
                            conversion_error = Some(sql_err!(
                                sqlstate::DATATYPE_MISMATCH,
                                "foreign row has {} columns, expected {}",
                                remote.columns().len(),
                                table.n_columns
                            ));
                            return Ok(());
                        }
                        let mut values = [Datum::Null; crate::storage::MAX_COLUMNS];
                        for (column, raw) in remote.columns().iter().enumerate() {
                            values[column] = match decode_foreign_field(
                                storage, &table, column, *raw, txid, arena,
                            ) {
                                Ok(value) => value,
                                Err(error) => {
                                    conversion_error = Some(error);
                                    return Ok(());
                                }
                            };
                        }
                        let encoded = match super::exec::encode_projected_pub(
                            &values[..table.n_columns],
                            arena,
                        ) {
                            Ok(encoded) => encoded,
                            Err(error) => {
                                conversion_error = Some(error);
                                return Ok(());
                            }
                        };
                        if row_count == row_capacity {
                            let capacity = if row_capacity == 0 {
                                8
                            } else {
                                row_capacity * 2
                            };
                            let fresh = match arena.alloc_slice_with(capacity, |_| EMPTY) {
                                Ok(fresh) => fresh,
                                Err(_) => {
                                    conversion_error = Some(super::eval::arena_full());
                                    return Ok(());
                                }
                            };
                            if row_count != 0 {
                                let previous =
                                    unsafe { core::slice::from_raw_parts(rows, row_count) };
                                fresh[..row_count].copy_from_slice(previous);
                            }
                            rows = fresh.as_mut_ptr();
                            row_capacity = capacity;
                        }
                        unsafe { rows.add(row_count).write(encoded) };
                        row_count += 1;
                    }
                    ClientEvent::Sql(SqlEvent::Ready { .. }) => ready = true,
                    ClientEvent::Sql(
                        SqlEvent::CommandComplete { .. } | SqlEvent::RowDescription { .. },
                    ) => {}
                    _ => {
                        conversion_error = Some(sql_err!(
                            sqlstate::PROTOCOL_VIOLATION,
                            "unexpected foreign PostgreSQL response"
                        ));
                    }
                }
                Ok(())
            })?;
        }
        Ok(())
    })();
    drop(client);
    if let Err(error) = execution {
        abort_session(storage, txid);
        return Err(error);
    }
    if let Some(error) = conversion_error {
        return Err(error);
    }
    Ok(if row_count == 0 {
        &[]
    } else {
        unsafe { core::slice::from_raw_parts(rows, row_count) }
    })
}

/// Visits every remote row with its parsed PostgreSQL tuple identity.  The
/// identity is selected separately from user columns, then converted into the
/// bounded physical-row representation consumed by local DML predicates.
/// It never becomes a fabricated local heap row identifier.
pub(crate) fn visit_mutable_rows(
    storage: &Storage,
    table_slot: usize,
    txid: u32,
    arena: &Arena,
    visit: &mut impl FnMut(RemoteTupleId, &[u8]) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    let (endpoint, table, foreign, timeout) = endpoint(storage, table_slot, txid)?;
    let remote_schema = foreign
        .options
        .get("schema_name")
        .unwrap_or(table.schema.as_str());
    let remote_table = foreign
        .options
        .get("table_name")
        .unwrap_or(table.name.as_str());
    let mut query = StackStr::<16_384>::new();
    let _ = query.write_str("SELECT ctid::text");
    for (column, definition) in table.columns().iter().enumerate() {
        let _ = query.write_str(", ");
        quote_remote_column(&mut query, &foreign, column, definition);
    }
    let _ = query.write_str(" FROM ");
    quote_identifier(&mut query, remote_schema);
    let _ = query.write_char('.');
    quote_identifier(&mut query, remote_table);
    if query.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "foreign mutable-row query exceeds its fixed buffer"
        ));
    }

    enter_session(storage, txid, endpoint, timeout)?;
    let mut ready = false;
    let mut response_error = None;
    let mut client = storage.foreign_client()?;
    let execution = (|| -> Result<(), SqlError> {
        client.query(query.as_str()).map_err(client_error)?;
        let deadline = Instant::now() + timeout;
        while !ready && response_error.is_none() {
            poll_client(&mut client, deadline, &mut |event| {
                match event {
                    ClientEvent::Sql(SqlEvent::RowDescription { fields })
                        if fields as usize != table.n_columns + 1 =>
                    {
                        response_error = Some(sql_err!(
                            sqlstate::PROTOCOL_VIOLATION,
                            "foreign mutable-row scan returned {} columns, expected {}",
                            fields,
                            table.n_columns + 1
                        ));
                    }
                    ClientEvent::Sql(SqlEvent::DataRow(row)) => {
                        if row.columns().len() != table.n_columns + 1 {
                            response_error = Some(sql_err!(
                                sqlstate::PROTOCOL_VIOLATION,
                                "foreign mutable-row scan returned an invalid row shape"
                            ));
                            return Ok(());
                        }
                        let tuple_id = match row.columns()[0] {
                            Some(ctid) => match RemoteTupleId::parse(ctid) {
                                Ok(tuple_id) => tuple_id,
                                Err(error) => {
                                    response_error = Some(error);
                                    return Ok(());
                                }
                            },
                            None => {
                                response_error = Some(sql_err!(
                                    sqlstate::PROTOCOL_VIOLATION,
                                    "foreign mutable-row scan returned a NULL ctid"
                                ));
                                return Ok(());
                            }
                        };
                        let mut values = [Datum::Null; crate::storage::MAX_COLUMNS];
                        for (column, raw) in row.columns()[1..].iter().enumerate() {
                            values[column] = match decode_foreign_field(
                                storage, &table, column, *raw, txid, arena,
                            ) {
                                Ok(value) => value,
                                Err(error) => {
                                    response_error = Some(error);
                                    return Ok(());
                                }
                            };
                        }
                        let len = rowenc::encoded_len(&values[..table.n_columns]);
                        let encoded = match arena.alloc_slice_with(len, |_| 0u8) {
                            Ok(encoded) => {
                                rowenc::encode(&values[..table.n_columns], encoded);
                                &*encoded
                            }
                            Err(_) => {
                                response_error = Some(super::eval::arena_full());
                                return Ok(());
                            }
                        };
                        if let Err(error) = visit(tuple_id, encoded) {
                            response_error = Some(error);
                        }
                    }
                    ClientEvent::Sql(SqlEvent::Ready {
                        transaction_status: b'T',
                    }) => ready = true,
                    ClientEvent::Sql(
                        SqlEvent::CommandComplete { .. } | SqlEvent::RowDescription { .. },
                    ) => {}
                    _ => {
                        response_error = Some(sql_err!(
                            sqlstate::PROTOCOL_VIOLATION,
                            "unexpected foreign PostgreSQL mutable-row response"
                        ));
                    }
                }
                Ok(())
            })?;
        }
        Ok(())
    })();
    drop(client);
    if let Err(error) = execution {
        abort_session(storage, txid);
        return Err(error);
    }
    response_error.map_or(Ok(()), Err)
}

/// Inserts one fully evaluated local row and replaces `values` with the remote
/// `RETURNING` image.  PostgreSQL's unnamed extended query keeps values as
/// length-delimited protocol fields; only catalog-derived identifiers enter
/// the SQL source.
pub(crate) fn insert_row<'a>(
    storage: &Storage,
    table_slot: usize,
    txid: u32,
    values: &mut [Datum<'a>],
    on_conflict_do_nothing: bool,
    render: crate::sql::guc::RenderContext,
    arena: &'a Arena,
) -> Result<Option<RemoteTupleId>, SqlError> {
    let (endpoint, table, foreign, timeout) = endpoint(storage, table_slot, txid)?;
    let remote_schema = foreign
        .options
        .get("schema_name")
        .unwrap_or(table.schema.as_str());
    let remote_table = foreign
        .options
        .get("table_name")
        .unwrap_or(table.name.as_str());
    let mut query = StackStr::<16_384>::new();
    let _ = query.write_str("INSERT INTO ");
    quote_identifier(&mut query, remote_schema);
    let _ = query.write_char('.');
    quote_identifier(&mut query, remote_table);
    let _ = query.write_str(" (");
    for (column, definition) in table.columns().iter().enumerate() {
        if column != 0 {
            let _ = query.write_str(", ");
        }
        let remote = foreign
            .column_options
            .options_for(column as u16)
            .find(|option| option.name.as_str() == "column_name");
        match remote {
            Some(option) => quote_identifier(&mut query, option.value.as_str()),
            None => quote_identifier(&mut query, definition.name.as_str()),
        }
    }
    let _ = query.write_str(") VALUES (");
    for column in 0..table.n_columns {
        if column != 0 {
            let _ = query.write_str(", ");
        }
        let _ = write!(query, "${}", column + 1);
    }
    let _ = query.write_char(')');
    if on_conflict_do_nothing {
        let _ = query.write_str(" ON CONFLICT DO NOTHING");
    }
    let _ = query.write_str(" RETURNING ctid::text");
    for (column, definition) in table.columns().iter().enumerate() {
        let _ = query.write_str(", ");
        let remote = foreign
            .column_options
            .options_for(column as u16)
            .find(|option| option.name.as_str() == "column_name");
        match remote {
            Some(option) => quote_identifier(&mut query, option.value.as_str()),
            None => quote_identifier(&mut query, definition.name.as_str()),
        }
    }
    if query.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "foreign INSERT query exceeds its fixed buffer"
        ));
    }

    let mut parameters = [None; crate::storage::MAX_COLUMNS];
    for (column, value) in values.iter().enumerate() {
        parameters[column] = Responder::datum_wire_text(value, render, arena)?;
    }
    enter_session(storage, txid, endpoint, timeout)?;
    let mut returned = None;
    let mut response_error = None;
    let mut ready = false;
    let mut client = storage.foreign_client()?;
    let execution = (|| -> Result<(), SqlError> {
        client
            .query_params(query.as_str(), &parameters[..table.n_columns])
            .map_err(client_error)?;
        let deadline = Instant::now() + timeout;
        while !ready && response_error.is_none() {
            poll_client(&mut client, deadline, &mut |event| {
                match event {
                    ClientEvent::Sql(SqlEvent::RowDescription { fields })
                        if fields as usize != table.n_columns + 1 =>
                    {
                        response_error = Some(sql_err!(
                            sqlstate::PROTOCOL_VIOLATION,
                            "foreign INSERT returned {} columns, expected {}",
                            fields,
                            table.n_columns + 1
                        ));
                    }
                    ClientEvent::Sql(SqlEvent::DataRow(row)) => {
                        if returned.is_some() || row.columns().len() != table.n_columns + 1 {
                            response_error = Some(sql_err!(
                                sqlstate::PROTOCOL_VIOLATION,
                                "foreign INSERT returned an invalid row count or shape"
                            ));
                            return Ok(());
                        }
                        let tuple_id = match row.columns()[0] {
                            Some(raw) => match RemoteTupleId::parse(raw) {
                                Ok(tuple_id) => tuple_id,
                                Err(error) => {
                                    response_error = Some(error);
                                    return Ok(());
                                }
                            },
                            None => {
                                response_error = Some(sql_err!(
                                    sqlstate::PROTOCOL_VIOLATION,
                                    "foreign INSERT returned a NULL ctid"
                                ));
                                return Ok(());
                            }
                        };
                        for (column, raw) in row.columns()[1..].iter().enumerate() {
                            let raw = match raw {
                                Some(raw) => match arena.alloc_slice_copy(raw) {
                                    Ok(raw) => Some(&*raw),
                                    Err(_) => {
                                        response_error = Some(super::eval::arena_full());
                                        return Ok(());
                                    }
                                },
                                None => None,
                            };
                            values[column] = match decode_foreign_field(
                                storage, &table, column, raw, txid, arena,
                            ) {
                                Ok(value) => value,
                                Err(error) => {
                                    response_error = Some(error);
                                    return Ok(());
                                }
                            };
                        }
                        returned = Some(tuple_id);
                    }
                    ClientEvent::Sql(SqlEvent::Ready {
                        transaction_status: b'T',
                    }) => ready = true,
                    ClientEvent::Sql(
                        SqlEvent::CommandComplete { .. } | SqlEvent::RowDescription { .. },
                    ) => {}
                    _ => {
                        response_error = Some(sql_err!(
                            sqlstate::PROTOCOL_VIOLATION,
                            "unexpected foreign PostgreSQL INSERT response"
                        ));
                    }
                }
                Ok(())
            })?;
        }
        Ok(())
    })();
    drop(client);
    if let Err(error) = execution {
        abort_session(storage, txid);
        return Err(error);
    }
    if let Some(error) = response_error {
        return Err(error);
    }
    Ok(returned)
}

fn remote_returning_row<'a>(
    storage: &Storage,
    table: &TableDef,
    txid: u32,
    arena: &'a Arena,
    query: &str,
    parameters: &[Option<&[u8]>],
    operation: &str,
) -> Result<Option<&'a [u8]>, SqlError> {
    let mut returned = None;
    let mut response_error = None;
    let mut ready = false;
    let mut client = storage.foreign_client()?;
    let timeout = storage.foreign_session_timeout(txid).ok_or_else(|| {
        sql_err!(
            sqlstate::INTERNAL_ERROR,
            "foreign mutation has no transaction-owned remote session"
        )
    })?;
    let execution = (|| -> Result<(), SqlError> {
        client
            .query_params(query, parameters)
            .map_err(client_error)?;
        let deadline = Instant::now() + timeout;
        while !ready && response_error.is_none() {
            poll_client(&mut client, deadline, &mut |event| {
                match event {
                    ClientEvent::Sql(SqlEvent::RowDescription { fields })
                        if fields as usize != table.n_columns =>
                    {
                        response_error = Some(sql_err!(
                            sqlstate::PROTOCOL_VIOLATION,
                            "foreign {} returned {} columns, expected {}",
                            operation,
                            fields,
                            table.n_columns
                        ));
                    }
                    ClientEvent::Sql(SqlEvent::DataRow(row)) => {
                        if returned.is_some() || row.columns().len() != table.n_columns {
                            response_error = Some(sql_err!(
                                sqlstate::PROTOCOL_VIOLATION,
                                "foreign {} returned an invalid row count or shape",
                                operation
                            ));
                            return Ok(());
                        }
                        let mut values = [Datum::Null; crate::storage::MAX_COLUMNS];
                        for (column, raw) in row.columns().iter().enumerate() {
                            values[column] = match decode_foreign_field(
                                storage, table, column, *raw, txid, arena,
                            ) {
                                Ok(value) => value,
                                Err(error) => {
                                    response_error = Some(error);
                                    return Ok(());
                                }
                            };
                        }
                        let len = rowenc::encoded_len(&values[..table.n_columns]);
                        returned = match arena.alloc_slice_with(len, |_| 0u8) {
                            Ok(row) => {
                                rowenc::encode(&values[..table.n_columns], row);
                                Some(&*row)
                            }
                            Err(_) => {
                                response_error = Some(super::eval::arena_full());
                                None
                            }
                        };
                    }
                    ClientEvent::Sql(SqlEvent::Ready {
                        transaction_status: b'T',
                    }) => ready = true,
                    ClientEvent::Sql(
                        SqlEvent::CommandComplete { .. } | SqlEvent::RowDescription { .. },
                    ) => {}
                    _ => {
                        response_error = Some(sql_err!(
                            sqlstate::PROTOCOL_VIOLATION,
                            "unexpected foreign PostgreSQL {} response",
                            operation
                        ));
                    }
                }
                Ok(())
            })?;
        }
        Ok(())
    })();
    drop(client);
    if let Err(error) = execution {
        abort_session(storage, txid);
        return Err(error);
    }
    response_error.map_or(Ok(returned), Err)
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
) -> Result<Option<&'a [u8]>, SqlError> {
    let (endpoint, table, foreign, timeout) = endpoint(storage, table_slot, txid)?;
    let remote_schema = foreign
        .options
        .get("schema_name")
        .unwrap_or(table.schema.as_str());
    let remote_table = foreign
        .options
        .get("table_name")
        .unwrap_or(table.name.as_str());
    let mut query = StackStr::<16_384>::new();
    let _ = query.write_str("SELECT ");
    for (column, definition) in table.columns().iter().enumerate() {
        if column != 0 {
            let _ = query.write_str(", ");
        }
        quote_remote_column(&mut query, &foreign, column, definition);
    }
    let _ = query.write_str(" FROM ");
    quote_identifier(&mut query, remote_schema);
    let _ = query.write_char('.');
    quote_identifier(&mut query, remote_table);
    let _ = query.write_str(" WHERE ctid = $1::tid");
    if query.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "foreign row fetch query exceeds its fixed buffer"
        ));
    }
    let mut parameter = StackStr::<32>::new();
    tuple_id.write_text(&mut parameter);
    enter_session(storage, txid, endpoint, timeout)?;
    remote_returning_row(
        storage,
        &table,
        txid,
        arena,
        query.as_str(),
        &[Some(parameter.as_str().as_bytes())],
        "row fetch",
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
) -> Result<bool, SqlError> {
    let RemoteUpdate {
        tuple_id,
        target_columns,
        values,
        render,
    } = request;
    let (endpoint, table, foreign, timeout) = endpoint(storage, table_slot, txid)?;
    let remote_schema = foreign
        .options
        .get("schema_name")
        .unwrap_or(table.schema.as_str());
    let remote_table = foreign
        .options
        .get("table_name")
        .unwrap_or(table.name.as_str());
    let mut query = StackStr::<16_384>::new();
    let _ = query.write_str("UPDATE ");
    quote_identifier(&mut query, remote_schema);
    let _ = query.write_char('.');
    quote_identifier(&mut query, remote_table);
    let _ = query.write_str(" SET ");
    for (parameter, &column) in target_columns.iter().enumerate() {
        if parameter != 0 {
            let _ = query.write_str(", ");
        }
        let definition = &table.columns()[column];
        quote_remote_column(&mut query, &foreign, column, definition);
        let _ = write!(query, " = ${}", parameter + 1);
    }
    let _ = write!(
        query,
        " WHERE ctid = ${}::tid RETURNING ",
        target_columns.len() + 1
    );
    for (column, definition) in table.columns().iter().enumerate() {
        if column != 0 {
            let _ = query.write_str(", ");
        }
        quote_remote_column(&mut query, &foreign, column, definition);
    }
    if query.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "foreign UPDATE query exceeds its fixed buffer"
        ));
    }
    let mut parameters = [None; crate::storage::MAX_COLUMNS + 1];
    for (parameter, &column) in target_columns.iter().enumerate() {
        parameters[parameter] = Responder::datum_wire_text(&values[column], render, arena)?;
    }
    let mut identity = StackStr::<32>::new();
    tuple_id.write_text(&mut identity);
    parameters[target_columns.len()] = Some(identity.as_str().as_bytes());
    enter_session(storage, txid, endpoint, timeout)?;
    let Some(row) = remote_returning_row(
        storage,
        &table,
        txid,
        arena,
        query.as_str(),
        &parameters[..target_columns.len() + 1],
        "UPDATE",
    )?
    else {
        return Ok(false);
    };
    let mut schema = [crate::sql::types::ColType::Bool; crate::storage::MAX_COLUMNS];
    table.schema(&mut schema);
    rowenc::decode(row, &schema[..table.n_columns], values)?;
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
) -> Result<Option<&'a [u8]>, SqlError> {
    let (endpoint, table, foreign, timeout) = endpoint(storage, table_slot, txid)?;
    let remote_schema = foreign
        .options
        .get("schema_name")
        .unwrap_or(table.schema.as_str());
    let remote_table = foreign
        .options
        .get("table_name")
        .unwrap_or(table.name.as_str());
    let mut query = StackStr::<16_384>::new();
    let _ = query.write_str("DELETE FROM ");
    quote_identifier(&mut query, remote_schema);
    let _ = query.write_char('.');
    quote_identifier(&mut query, remote_table);
    let _ = query.write_str(" WHERE ctid = $1::tid RETURNING ");
    for (column, definition) in table.columns().iter().enumerate() {
        if column != 0 {
            let _ = query.write_str(", ");
        }
        quote_remote_column(&mut query, &foreign, column, definition);
    }
    if query.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "foreign DELETE query exceeds its fixed buffer"
        ));
    }
    let mut parameter = StackStr::<32>::new();
    tuple_id.write_text(&mut parameter);
    enter_session(storage, txid, endpoint, timeout)?;
    remote_returning_row(
        storage,
        &table,
        txid,
        arena,
        query.as_str(),
        &[Some(parameter.as_str().as_bytes())],
        "DELETE",
    )
}

fn execute_command(
    storage: &Storage,
    txid: u32,
    endpoint: ConnectionInfo,
    timeout: Duration,
    command: &str,
) -> Result<(), SqlError> {
    enter_session(storage, txid, endpoint, timeout)?;
    let mut ready = false;
    let mut complete = false;
    let mut client = storage.foreign_client()?;
    let execution = (|| -> Result<(), SqlError> {
        client.query(command).map_err(client_error)?;
        let deadline = Instant::now() + timeout;
        while !ready {
            poll_client(&mut client, deadline, &mut |event| {
                match event {
                    ClientEvent::Sql(SqlEvent::CommandComplete { .. }) if !complete => {
                        complete = true
                    }
                    ClientEvent::Sql(SqlEvent::Ready {
                        transaction_status: b'T',
                    }) if complete => ready = true,
                    _ => {
                        return Err(ClientError::Protocol(
                            crate::pg::replication_client::FrameError::Malformed,
                        ));
                    }
                }
                Ok(())
            })?;
        }
        Ok(())
    })();
    drop(client);
    if let Err(error) = execution {
        abort_session(storage, txid);
        return Err(error);
    }
    Ok(())
}

pub(crate) fn truncate_tables(
    storage: &Storage,
    table_slots: &[usize],
    txid: u32,
    restart_identity: bool,
    cascade: bool,
) -> Result<(), SqlError> {
    let Some((&first, rest)) = table_slots.split_first() else {
        return Ok(());
    };
    let (connection, _, _, timeout) = endpoint(storage, first, txid)?;
    let mut query = StackStr::<16_384>::new();
    let _ = query.write_str("TRUNCATE TABLE ");
    for (index, slot) in core::iter::once(&first).chain(rest.iter()).enumerate() {
        let (candidate_endpoint, table, foreign, _) = endpoint(storage, *slot, txid)?;
        if candidate_endpoint != connection {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "TRUNCATE of foreign tables on different servers is not supported"
            ));
        }
        if index != 0 {
            let _ = query.write_str(", ");
        }
        let schema = foreign
            .options
            .get("schema_name")
            .unwrap_or(table.schema.as_str());
        let name = foreign
            .options
            .get("table_name")
            .unwrap_or(table.name.as_str());
        quote_identifier(&mut query, schema);
        let _ = query.write_char('.');
        quote_identifier(&mut query, name);
    }
    if restart_identity {
        let _ = query.write_str(" RESTART IDENTITY");
    } else {
        let _ = query.write_str(" CONTINUE IDENTITY");
    }
    if cascade {
        let _ = query.write_str(" CASCADE");
    } else {
        let _ = query.write_str(" RESTRICT");
    }
    if query.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "foreign TRUNCATE query exceeds its fixed buffer"
        ));
    }
    execute_command(storage, txid, connection, timeout, query.as_str())
}

#[cfg(test)]
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
