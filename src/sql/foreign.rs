//! Bounded postgres_fdw transport and row materialization.

use core::fmt::Write as _;
use std::time::{Duration, Instant};

use crate::mem::arena::Arena;
use crate::pg::replication_client::{ClientError, ClientEvent, ConnectionInfo, SqlEvent, SslMode};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::types::Datum;
use crate::sql_err;
use crate::storage::foreign::{ForeignDataHandler, ForeignMappingUser, ForeignServerDefinition};
use crate::storage::{PrivilegeSet, Storage, TableDef};
use crate::util::StackStr;

const DEFAULT_CONNECT_TIMEOUT_SECONDS: u64 = 10;

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
    let mut ready = false;
    let mut client = storage.foreign_client()?;
    let execution = (|| -> Result<(), SqlError> {
        client.bind_sql(endpoint).map_err(client_error)?;
        let deadline = Instant::now() + timeout;
        while !ready {
            poll_client(&mut client, deadline, &mut |event| {
                if matches!(event, ClientEvent::Sql(SqlEvent::Ready { .. })) {
                    ready = true;
                }
                Ok(())
            })?;
        }
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
    client.unbind();
    execution?;
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
    let mut ready = false;
    let mut client = storage.foreign_client()?;
    let execution = (|| -> Result<(), SqlError> {
        client.bind_sql(endpoint).map_err(client_error)?;
        let deadline = Instant::now() + timeout;
        while !ready {
            poll_client(&mut client, deadline, &mut |event| {
                if matches!(event, ClientEvent::Sql(SqlEvent::Ready { .. })) {
                    ready = true;
                }
                Ok(())
            })?;
        }
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
                            values[column] = match raw {
                                None => Datum::Null,
                                Some(raw) => {
                                    let oid = match storage
                                        .declared_column_type(&table.columns()[column], txid)
                                    {
                                        Ok(declared) => declared.catalog_oid(),
                                        Err(error) => {
                                            conversion_error = Some(error);
                                            return Ok(());
                                        }
                                    };
                                    match super::exec::decode_text_input(
                                        storage, oid, raw, arena, txid,
                                    ) {
                                        Ok(value) => value,
                                        Err(error) => {
                                            conversion_error = Some(error);
                                            return Ok(());
                                        }
                                    }
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
    client.unbind();
    execution?;
    if let Some(error) = conversion_error {
        return Err(error);
    }
    Ok(if row_count == 0 {
        &[]
    } else {
        unsafe { core::slice::from_raw_parts(rows, row_count) }
    })
}
