//! postgres_fdw's remote command text, cursor scans, batches and connection
//! cache, against a scripted PostgreSQL server that records each command.

use super::*;
use crate::mem::mutex::Mutex;
use std::io::{Read, Write};
use std::sync::Arc;

/// The commands the scripted server received, one entry per connection.
type Log = Arc<Mutex<Vec<Vec<String>>>>;

fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![kind];
    out.extend_from_slice(&((payload.len() + 4) as i32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn complete(tag: &str) -> Vec<u8> {
    let mut payload = tag.as_bytes().to_vec();
    payload.push(0);
    frame(b'C', &payload)
}

fn ready(status: u8) -> Vec<u8> {
    frame(b'Z', &[status])
}

fn cstr(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|byte| *byte == 0).unwrap();
    String::from_utf8(bytes[..end].to_vec()).unwrap()
}

/// A RowDescription of one `integer` column named `id`.
fn id_description() -> Vec<u8> {
    let mut description = 1i16.to_be_bytes().to_vec();
    description.extend_from_slice(b"id\0");
    description.extend_from_slice(&[0; 6]);
    description.extend_from_slice(&23i32.to_be_bytes());
    description.extend_from_slice(&4i16.to_be_bytes());
    description.extend_from_slice(&(-1i32).to_be_bytes());
    description.extend_from_slice(&0i16.to_be_bytes());
    frame(b'T', &description)
}

fn id_row(value: i32) -> Vec<u8> {
    let text = value.to_string();
    let mut row = 1i16.to_be_bytes().to_vec();
    row.extend_from_slice(&(text.len() as i32).to_be_bytes());
    row.extend_from_slice(text.as_bytes());
    frame(b'D', &row)
}

/// The reply to one command against a remote table of `rows` integers.
fn respond(command: &str, rows: &mut std::ops::Range<i32>, status: &mut u8) -> Vec<u8> {
    let mut out = Vec::new();
    if command.starts_with("SET ") {
        out.extend(complete("SET"));
    } else if command.starts_with("START TRANSACTION") {
        *status = b'T';
        out.extend(complete("START TRANSACTION"));
    } else if command == "COMMIT TRANSACTION" {
        *status = b'I';
        out.extend(complete("COMMIT"));
    } else if command == "ABORT TRANSACTION" {
        *status = b'I';
        out.extend(complete("ROLLBACK"));
    } else if command.starts_with("DECLARE ") {
        *rows = 0..5;
        out.extend(complete("DECLARE CURSOR"));
    } else if let Some(rest) = command.strip_prefix("FETCH ") {
        let size: i32 = rest.split(' ').next().unwrap().parse().unwrap();
        out.extend(id_description());
        let mut fetched = 0;
        while fetched < size {
            let Some(value) = rows.next() else { break };
            out.extend(id_row(value));
            fetched += 1;
        }
        out.extend(complete(&format!("FETCH {fetched}")));
    } else if command.starts_with("CLOSE ") {
        out.extend(complete("CLOSE CURSOR"));
    } else if command.starts_with("INSERT ") {
        let values = command.matches("($").count();
        if command.contains(" RETURNING ") {
            out.extend(id_description());
            out.extend(id_row(7));
        }
        out.extend(complete(&format!("INSERT 0 {values}")));
    } else {
        panic!("unscripted remote command {command:?}");
    }
    out
}

/// Serves connections until the listener's test ends, logging commands.
fn scripted_server() -> (u16, Log) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let connections = Arc::clone(&log);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let index = {
                let mut log = connections.lock().unwrap();
                log.push(Vec::new());
                log.len() - 1
            };
            let log = Arc::clone(&connections);
            std::thread::spawn(move || {
                let mut length = [0u8; 4];
                stream.read_exact(&mut length).unwrap();
                let mut startup = vec![0u8; i32::from_be_bytes(length) as usize - 4];
                stream.read_exact(&mut startup).unwrap();
                let mut greeting = frame(b'R', &0i32.to_be_bytes());
                let mut key = 4242i32.to_be_bytes().to_vec();
                key.extend_from_slice(&7i32.to_be_bytes());
                greeting.extend(frame(b'K', &key));
                greeting.extend(ready(b'I'));
                stream.write_all(&greeting).unwrap();
                let mut status = b'I';
                let mut rows = 0..0;
                let mut parsed = String::new();
                loop {
                    let mut head = [0u8; 5];
                    if stream.read_exact(&mut head).is_err() {
                        return;
                    }
                    let mut body =
                        vec![0u8; i32::from_be_bytes(head[1..5].try_into().unwrap()) as usize - 4];
                    stream.read_exact(&mut body).unwrap();
                    let reply = match head[0] {
                        b'Q' => {
                            let command = cstr(&body);
                            log.lock().unwrap()[index].push(command.clone());
                            let mut reply = respond(&command, &mut rows, &mut status);
                            reply.extend(ready(status));
                            reply
                        }
                        b'P' => {
                            parsed = cstr(&body[1..]);
                            log.lock().unwrap()[index].push(parsed.clone());
                            frame(b'1', &[])
                        }
                        b'B' => frame(b'2', &[]),
                        b'E' => respond(&parsed, &mut rows, &mut status),
                        b'S' => ready(status),
                        b'X' => return,
                        other => panic!("unscripted frontend message {other}"),
                    };
                    stream.write_all(&reply).unwrap();
                }
            });
        }
    });
    (port, log)
}

fn commands(log: &Log) -> Vec<Vec<String>> {
    log.lock().unwrap().clone()
}

#[test]
fn postgres_fdw_caches_connections_and_spells_its_remote_commands() {
    let (port, log) = scripted_server();
    let (mut engine, mut budget) = test_engine();
    let setup = run_with(
        &mut engine,
        &mut budget,
        &format!(
            "CREATE EXTENSION postgres_fdw; \
             CREATE SERVER scripted FOREIGN DATA WRAPPER postgres_fdw \
               OPTIONS (host '127.0.0.1', port '{port}', dbname 'remote', \
                        sslmode 'disable', fetch_size '2'); \
             CREATE USER MAPPING FOR CURRENT_USER SERVER scripted OPTIONS (user 'remote'); \
             CREATE FOREIGN TABLE ft (id integer) SERVER scripted \
               OPTIONS (schema_name 'public', table_name 'ft', batch_size '2')"
        ),
    );
    assert!(
        !message_types(&setup).contains(&b'E'),
        "{}",
        String::from_utf8_lossy(&setup)
    );
    let scanned = run_with(&mut engine, &mut budget, "SELECT sum(id) FROM ft");
    assert_eq!(data_rows(&scanned), ["10"]);
    let scanned = run_with(&mut engine, &mut budget, "SELECT count(*) FROM ft");
    assert_eq!(data_rows(&scanned), ["5"]);
    let inserted = run_with(
        &mut engine,
        &mut budget,
        "INSERT INTO ft VALUES (1), (2), (3)",
    );
    assert!(
        String::from_utf8_lossy(&inserted).contains("INSERT 0 3"),
        "{}",
        String::from_utf8_lossy(&inserted)
    );
    let cached = run_with(
        &mut engine,
        &mut budget,
        "SELECT server_name, user_name, valid, used_in_xact, remote_backend_pid \
         FROM postgres_fdw_get_connections()",
    );
    assert_eq!(data_rows(&cached), ["scripted|postgres|t|f|4242"]);
    let configure = [
        "SET search_path = pg_catalog",
        "SET timezone = 'UTC'",
        "SET datestyle = ISO",
        "SET intervalstyle = postgres",
        "SET extra_float_digits = 3",
    ];
    let scan = |cursor: u32| {
        [
            format!("DECLARE c{cursor} CURSOR FOR\nSELECT id FROM public.ft"),
            format!("FETCH 2 FROM c{cursor}"),
            format!("FETCH 2 FROM c{cursor}"),
            format!("FETCH 2 FROM c{cursor}"),
            format!("CLOSE c{cursor}"),
        ]
    };
    let mut expected: Vec<String> = configure.iter().map(|text| text.to_string()).collect();
    for _ in 0..2 {
        expected.push("START TRANSACTION ISOLATION LEVEL REPEATABLE READ".into());
        expected.extend(scan(1));
        expected.push("COMMIT TRANSACTION".into());
    }
    expected.push("START TRANSACTION ISOLATION LEVEL REPEATABLE READ".into());
    expected.push("INSERT INTO public.ft(id) VALUES ($1), ($2)".into());
    expected.push("INSERT INTO public.ft(id) VALUES ($1)".into());
    expected.push("COMMIT TRANSACTION".into());
    assert_eq!(commands(&log), [expected]);

    // A server change closes the idle connection; the next use reopens it.
    let altered = run_with(
        &mut engine,
        &mut budget,
        "ALTER SERVER scripted OPTIONS (ADD keep_connections 'off'); \
         SELECT server_name FROM postgres_fdw_get_connections()",
    );
    assert!(data_rows(&altered).is_empty());
    let returning = run_with(
        &mut engine,
        &mut budget,
        "INSERT INTO ft VALUES (7) RETURNING id",
    );
    assert_eq!(data_rows(&returning), ["7"]);
    let after = commands(&log);
    assert_eq!(after.len(), 2);
    assert_eq!(
        after[1][configure.len() + 1],
        "INSERT INTO public.ft(id) VALUES ($1) RETURNING id"
    );
    let closed = run_with(
        &mut engine,
        &mut budget,
        "SELECT count(*) FROM postgres_fdw_get_connections()",
    );
    assert_eq!(data_rows(&closed), ["0"]);
}

#[test]
fn foreign_tables_refuse_merge_and_transition_tables() {
    let (mut engine, mut budget) = test_engine();
    let setup = run_with(
        &mut engine,
        &mut budget,
        "CREATE FOREIGN DATA WRAPPER refusing; \
         CREATE SERVER refusing_server FOREIGN DATA WRAPPER refusing; \
         CREATE FOREIGN TABLE refused (id integer) SERVER refusing_server; \
         CREATE FUNCTION refused_audit() RETURNS trigger LANGUAGE plpgsql \
           AS $$ BEGIN RETURN NULL; END $$",
    );
    assert!(!message_types(&setup).contains(&b'E'));
    for (statement, expected) in [
        (
            "MERGE INTO refused USING (VALUES (1)) v(x) ON id = x WHEN MATCHED THEN DELETE",
            "cannot execute MERGE on relation \"refused\"",
        ),
        (
            "CREATE TRIGGER refused_audit AFTER INSERT ON refused REFERENCING NEW TABLE AS n \
             FOR EACH STATEMENT EXECUTE FUNCTION refused_audit()",
            "Triggers on foreign tables cannot have transition tables.",
        ),
        (
            "ALTER SERVER refusing_server OPTIONS (ADD a '1', ADD a '2')",
            "option \"a\" provided more than once",
        ),
    ] {
        let output = run_with(&mut engine, &mut budget, statement);
        let rendered = String::from_utf8_lossy(&output);
        assert!(rendered.contains(expected), "{statement}: {rendered}");
    }
}
