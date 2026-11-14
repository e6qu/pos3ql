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

/// A RowDescription of one `tid` column named `ctid`.
fn ctid_description() -> Vec<u8> {
    let mut description = 1i16.to_be_bytes().to_vec();
    description.extend_from_slice(b"ctid\0");
    description.extend_from_slice(&[0; 6]);
    description.extend_from_slice(&27i32.to_be_bytes());
    description.extend_from_slice(&6i16.to_be_bytes());
    description.extend_from_slice(&(-1i32).to_be_bytes());
    description.extend_from_slice(&0i16.to_be_bytes());
    frame(b'T', &description)
}

fn ctid_row(value: i32) -> Vec<u8> {
    let text = format!("(0,{value})");
    let mut row = 1i16.to_be_bytes().to_vec();
    row.extend_from_slice(&(text.len() as i32).to_be_bytes());
    row.extend_from_slice(text.as_bytes());
    frame(b'D', &row)
}

/// The reply to one command against a remote table of `rows` integers. A
/// cursor declared over `SELECT ctid` reads row identities instead.
fn respond_to(
    command: &str,
    rows: &mut std::ops::Range<i32>,
    status: &mut u8,
    identities: &mut bool,
) -> Vec<u8> {
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
        *identities = command.contains("\nSELECT ctid ");
        out.extend(complete("DECLARE CURSOR"));
    } else if let Some(rest) = command.strip_prefix("FETCH ") {
        let size: i32 = rest.split(' ').next().unwrap().parse().unwrap();
        out.extend(if *identities {
            ctid_description()
        } else {
            id_description()
        });
        let mut fetched = 0;
        while fetched < size {
            let Some(value) = rows.next() else { break };
            out.extend(if *identities {
                ctid_row(value + 1)
            } else {
                id_row(value)
            });
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
    } else if command.starts_with("UPDATE ") {
        out.extend(complete("UPDATE 2"));
    } else if command.starts_with("DELETE ") {
        out.extend(complete(if command.ends_with("ctid = $1") {
            "DELETE 1"
        } else {
            "DELETE 3"
        }));
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
                let mut identities = false;
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
                            let mut reply =
                                respond_to(&command, &mut rows, &mut status, &mut identities);
                            reply.extend(ready(status));
                            reply
                        }
                        b'P' => {
                            parsed = cstr(&body[1..]);
                            log.lock().unwrap()[index].push(parsed.clone());
                            frame(b'1', &[])
                        }
                        b'B' => frame(b'2', &[]),
                        b'E' => respond_to(&parsed, &mut rows, &mut status, &mut identities),
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
    for query in ["SELECT id FROM public.ft", "SELECT NULL FROM public.ft"] {
        expected.push("START TRANSACTION ISOLATION LEVEL REPEATABLE READ".into());
        expected.extend(scan(1).map(|command| command.replace("SELECT id FROM public.ft", query)));
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

/// The scripted server and an engine with a foreign table `ft (id integer)`
/// on it.
fn scripted_table() -> (Log, Engine, Budget) {
    let (port, log) = scripted_server();
    let (mut engine, mut budget) = test_engine();
    let setup = run_with(
        &mut engine,
        &mut budget,
        &format!(
            "CREATE EXTENSION postgres_fdw; \
             CREATE SERVER scripted FOREIGN DATA WRAPPER postgres_fdw \
               OPTIONS (host '127.0.0.1', port '{port}', dbname 'remote', sslmode 'disable'); \
             CREATE USER MAPPING FOR CURRENT_USER SERVER scripted OPTIONS (user 'remote'); \
             CREATE FOREIGN TABLE ft (id integer, note text) SERVER scripted \
               OPTIONS (schema_name 'public', table_name 'ft')"
        ),
    );
    assert!(
        !message_types(&setup).contains(&b'E'),
        "{}",
        String::from_utf8_lossy(&setup)
    );
    (log, engine, budget)
}

/// The statements after the session setup and transaction start.
fn remote_statements(log: &Log) -> Vec<String> {
    commands(log)
        .concat()
        .into_iter()
        .filter(|command| {
            !command.starts_with("SET ")
                && !command.starts_with("START TRANSACTION")
                && !command.starts_with("FETCH ")
                && !command.starts_with("CLOSE ")
                && command != "COMMIT TRANSACTION"
        })
        .collect()
}

#[test]
fn postgres_fdw_pushes_conditions_order_and_limit_and_modifies_directly() {
    let (log, mut engine, mut budget) = scripted_table();
    // Shipped conditions, order and limit are the server's work: the
    // scripted server applies none of them, so every row it sends arrives.
    let pushed = run_with(
        &mut engine,
        &mut budget,
        "SELECT id FROM ft WHERE id > 2 AND note <> 'x' ORDER BY id DESC LIMIT 2 OFFSET 1",
    );
    assert_eq!(data_rows(&pushed), ["0", "1", "2", "3", "4"]);
    // A volatile condition stays local and keeps the limit here.
    let local = run_with(
        &mut engine,
        &mut budget,
        "SELECT id FROM ft WHERE random() < 2 AND id = 3 LIMIT 2",
    );
    assert_eq!(data_rows(&local), ["0", "1"]);
    let updated = run_with(
        &mut engine,
        &mut budget,
        "UPDATE ft SET id = id + 1 WHERE id = 3",
    );
    assert!(String::from_utf8_lossy(&updated).contains("UPDATE 2"));
    let deleted = run_with(
        &mut engine,
        &mut budget,
        "DELETE FROM ft WHERE note IS NULL",
    );
    assert!(String::from_utf8_lossy(&deleted).contains("DELETE 3"));
    let by_row = run_with(
        &mut engine,
        &mut budget,
        "DELETE FROM ft WHERE id = 4 AND random() < 2",
    );
    assert!(
        String::from_utf8_lossy(&by_row).contains("DELETE 5"),
        "{}",
        String::from_utf8_lossy(&by_row)
    );
    let mut expected = vec![
        "DECLARE c1 CURSOR FOR\nSELECT id FROM public.ft WHERE ((id > 2)) AND ((note <> 'x')) \
         ORDER BY id DESC NULLS FIRST LIMIT 2::bigint OFFSET 1::bigint"
            .to_string(),
        "DECLARE c1 CURSOR FOR\nSELECT id FROM public.ft WHERE ((id = 3))".to_string(),
        "UPDATE public.ft SET id = (id + 1) WHERE ((id = 3))".to_string(),
        "DELETE FROM public.ft WHERE ((note IS NULL))".to_string(),
        "DECLARE c1 CURSOR FOR\nSELECT ctid FROM public.ft WHERE ((id = 4)) FOR UPDATE".to_string(),
    ];
    expected.extend((0..5).map(|_| "DELETE FROM public.ft WHERE ctid = $1".to_string()));
    assert_eq!(remote_statements(&log), expected);
}

#[test]
fn explain_verbose_shows_postgres_fdw_remote_sql() {
    let (_log, mut engine, mut budget) = scripted_table();
    let plan = |engine: &mut Engine, budget: &mut Budget, sql: &str| {
        data_rows(&run_with(
            engine,
            budget,
            &format!("EXPLAIN (VERBOSE, COSTS OFF) {sql}"),
        ))
    };
    assert_eq!(
        plan(
            &mut engine,
            &mut budget,
            "SELECT note FROM ft WHERE id < 3 AND random() > 0.5"
        ),
        [
            "Foreign Scan on public.ft",
            "  Output: note",
            "  Filter: (random() > '0.5'::double precision)",
            "  Remote SQL: SELECT note FROM public.ft WHERE ((id < 3))",
        ]
    );
    assert_eq!(
        plan(
            &mut engine,
            &mut budget,
            "SELECT id FROM ft ORDER BY note LIMIT 3"
        ),
        [
            "Foreign Scan on public.ft",
            "  Output: id, note",
            "  Remote SQL: SELECT id, note FROM public.ft ORDER BY note ASC NULLS LAST LIMIT 3::bigint",
        ]
    );
    assert_eq!(
        plan(
            &mut engine,
            &mut budget,
            "UPDATE ft SET note = 'y' WHERE id = 1"
        ),
        [
            "Update on public.ft",
            "  ->  Foreign Update on public.ft",
            "        Remote SQL: UPDATE public.ft SET note = 'y'::text WHERE ((id = 1))",
        ]
    );
    assert_eq!(
        plan(
            &mut engine,
            &mut budget,
            "DELETE FROM ft WHERE id = 1 RETURNING note"
        ),
        [
            "Delete on public.ft",
            "  Remote SQL: DELETE FROM public.ft WHERE ctid = $1 RETURNING note",
            "  ->  Foreign Scan on public.ft",
            "        Output: ctid",
            "        Remote SQL: SELECT ctid FROM public.ft WHERE ((id = 1)) FOR UPDATE",
        ]
    );
    assert_eq!(
        plan(&mut engine, &mut budget, "INSERT INTO ft VALUES (1, 'a')"),
        [
            "Insert on public.ft",
            "  Remote SQL: INSERT INTO public.ft(id, note) VALUES ($1, $2)",
            "  Batch Size: 1",
            "  ->  Result",
            "        Output: 1, 'a'::text",
        ]
    );
}

#[test]
fn foreign_scans_fetch_batches_as_the_plan_reads_them() {
    let (log, mut engine, mut budget) = scripted_table();
    let setup = run_with(
        &mut engine,
        &mut budget,
        "ALTER FOREIGN TABLE ft OPTIONS (ADD fetch_size '2')",
    );
    assert!(!message_types(&setup).contains(&b'E'));
    // A limit kept local reads one batch, then closes the cursor.
    let limited = run_with(
        &mut engine,
        &mut budget,
        "SELECT id FROM ft WHERE pg_typeof(id) IS NOT NULL LIMIT 1",
    );
    assert_eq!(data_rows(&limited), ["0"]);
    // A window's second pass reads the fetched rows again.
    let windowed = run_with(
        &mut engine,
        &mut budget,
        "SELECT id, count(*) OVER () FROM ft WHERE pg_typeof(id) IS NOT NULL ORDER BY id LIMIT 1",
    );
    assert_eq!(data_rows(&windowed), ["0|5"]);
    let fetches: Vec<String> = commands(&log)
        .concat()
        .into_iter()
        .filter(|command| {
            command.starts_with("DECLARE")
                || command.starts_with("FETCH")
                || command.starts_with("CLOSE")
        })
        .map(|command| command.lines().next().unwrap().to_string())
        .collect();
    assert_eq!(
        fetches,
        [
            "DECLARE c1 CURSOR FOR",
            "FETCH 2 FROM c1",
            "CLOSE c1",
            "DECLARE c1 CURSOR FOR",
            "FETCH 2 FROM c1",
            "FETCH 2 FROM c1",
            "FETCH 2 FROM c1",
            "CLOSE c1",
        ]
    );
}

#[test]
fn foreign_insert_batches_wait_through_routine_calls() {
    let (log, mut engine, mut budget) = scripted_table();
    let setup = run_with(
        &mut engine,
        &mut budget,
        "ALTER FOREIGN TABLE ft OPTIONS (ADD batch_size '3'); \
         CREATE FUNCTION label(i integer) RETURNS text LANGUAGE plpgsql \
           AS $$BEGIN RETURN 'p' || i; END$$",
    );
    assert!(
        !message_types(&setup).contains(&b'E'),
        "{}",
        String::from_utf8_lossy(&setup)
    );
    let inserted = run_with(
        &mut engine,
        &mut budget,
        "INSERT INTO ft SELECT i, label(i) FROM generate_series(1, 4) i",
    );
    assert!(
        String::from_utf8_lossy(&inserted).contains("INSERT 0 4"),
        "{}",
        String::from_utf8_lossy(&inserted)
    );
    // The calls pause the statement after each row; the queued rows still
    // go out as full batches and the remainder at the end.
    let batches: Vec<usize> = remote_statements(&log)
        .iter()
        .filter(|command| command.starts_with("INSERT "))
        .map(|command| command.matches("($").count())
        .collect();
    assert_eq!(batches, [3, 1]);
}
