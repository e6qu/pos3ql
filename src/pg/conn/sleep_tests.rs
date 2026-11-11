//! `pg_sleep` parks its statement on a timer: the SQL worker keeps serving
//! other connections, the statement resumes at its wake time without
//! repeating finished work, and statement_timeout and cancellation end it.
use super::*;

const KEY: &[u8; 16] = b"sleep-statements";

fn query(sql: &str) -> Vec<u8> {
    let mut message = vec![wire::FMSG_QUERY];
    message.extend_from_slice(&((sql.len() + 5) as i32).to_be_bytes());
    message.extend_from_slice(sql.as_bytes());
    message.push(0);
    message
}

struct Fixture {
    engine: Engine,
    budget: Budget,
    config: Config,
}

/// Each test owns its data directory: tests run concurrently in one process,
/// and the first to finish removes its directory.
fn fixture(name: &str) -> Fixture {
    let mut config = Config::default_dev();
    config.data_dir = std::env::temp_dir()
        .join(format!("pos3ql-sleep-{name}-{}", std::process::id()))
        .to_string_lossy()
        .into_owned();
    config.remove_test_data_dir_on_drop();
    config.max_tables = 4;
    config.max_connections = 3;
    let mut budget = crate::sql::test_engine_budget(&config, 1 << 30);
    let engine = Engine::new(&config, &mut budget).unwrap();
    Fixture {
        engine,
        budget,
        config,
    }
}

fn connection(fixture: &mut Fixture, id: i32) -> (Conn, TcpStream) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let server = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (client, _) = listener.accept().unwrap();
    server.set_nonblocking(true).unwrap();
    client.set_nonblocking(true).unwrap();
    let mut conn = Conn::new(&fixture.config, &mut fixture.budget).unwrap();
    conn.open(server, id);
    conn.auth_database = fixture.engine.database_login("postgres");
    conn.phase = Phase::Ready;
    (conn, client)
}

/// Sends `sql` and runs one reactor turn for it.
fn send(conn: &mut Conn, engine: &mut Engine, client: &mut TcpStream, sql: &str) {
    client.write_all(&query(sql)).unwrap();
    let mut poll = libc::pollfd {
        fd: conn.stream().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut poll, 1, 5000) }, 1);
    let auth = AuthContext {
        mode: AuthMode::Trust,
        password: String::new(),
        scram: None,
    };
    let pending = crate::mem::guard::forbid_alloc(|| conn.on_readable(engine, KEY, &auth, None));
    crate::mem::guard::forbid_alloc(|| conn.finish_response(pending, None));
}

/// Retries a parked connection as the reactor does after its wait timeout.
fn retry(conn: &mut Conn, engine: &mut Engine) -> bool {
    let generation = engine.lock_generation();
    match conn.retry_parked(engine, KEY, generation, false) {
        Some(pending) => {
            conn.finish_response(pending, None);
            true
        }
        None => false,
    }
}

/// Every complete backend message the client has received so far.
fn received(conn: &mut Conn, client: &mut TcpStream) -> Vec<(u8, Vec<u8>)> {
    assert_eq!(conn.on_writable(), After::Continue);
    std::thread::sleep(Duration::from_millis(20));
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match client.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => bytes.extend_from_slice(&chunk[..n]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("client read: {error}"),
        }
    }
    let mut messages = Vec::new();
    let mut rest = &bytes[..];
    while rest.len() >= 5 {
        let total = 1 + u32::from_be_bytes(rest[1..5].try_into().unwrap()) as usize;
        messages.push((rest[0], rest[5..total].to_vec()));
        rest = &rest[total..];
    }
    messages
}

/// Text of each data row, columns joined by `|` (NULL as empty).
fn rows(messages: &[(u8, Vec<u8>)]) -> Vec<String> {
    messages
        .iter()
        .filter(|(kind, _)| *kind == b'D')
        .map(|(_, payload)| {
            let count = i16::from_be_bytes(payload[..2].try_into().unwrap());
            let mut at = 2;
            let mut columns = Vec::new();
            for _ in 0..count {
                let length = i32::from_be_bytes(payload[at..at + 4].try_into().unwrap());
                at += 4;
                if length < 0 {
                    columns.push(String::new());
                } else {
                    let length = length as usize;
                    columns.push(String::from_utf8(payload[at..at + length].to_vec()).unwrap());
                    at += length;
                }
            }
            columns.join("|")
        })
        .collect()
}

fn sqlstates(messages: &[(u8, Vec<u8>)]) -> Vec<String> {
    messages
        .iter()
        .filter(|(kind, _)| *kind == b'E')
        .map(|(_, payload)| {
            let at = payload.iter().position(|byte| *byte == b'C').unwrap();
            let end = at + payload[at..].iter().position(|byte| *byte == 0).unwrap();
            String::from_utf8(payload[at + 1..end].to_vec()).unwrap()
        })
        .collect()
}

#[test]
fn sleeping_statement_parks_on_a_timer_while_other_sessions_run() {
    let mut fixture = fixture("timer");
    let (mut sleeper, mut sleeper_client) = connection(&mut fixture, 1);
    let (mut other, mut other_client) = connection(&mut fixture, 2);
    send(
        &mut sleeper,
        &mut fixture.engine,
        &mut sleeper_client,
        "CREATE SEQUENCE sleep_numbers",
    );
    received(&mut sleeper, &mut sleeper_client);
    let started = Instant::now();
    send(
        &mut sleeper,
        &mut fixture.engine,
        &mut sleeper_client,
        "SELECT nextval('sleep_numbers'), pg_sleep(0.3), nextval('sleep_numbers'), \
         pg_sleep_for('100 milliseconds')",
    );
    assert_eq!(
        sleeper.parked_wait(),
        Some(crate::sql::StatementWait::Timer)
    );
    assert!(sleeper.parked_wait_remaining().unwrap() <= Duration::from_millis(300));
    // The worker is free: another session runs to completion meanwhile.
    send(
        &mut other,
        &mut fixture.engine,
        &mut other_client,
        "SELECT 'other'",
    );
    assert_eq!(rows(&received(&mut other, &mut other_client)), ["other"]);
    assert!(!retry(&mut sleeper, &mut fixture.engine));
    assert_eq!(
        sleeper.parked_wait(),
        Some(crate::sql::StatementWait::Timer)
    );
    while sleeper.parked_wait().is_some() {
        std::thread::sleep(sleeper.parked_wait_remaining().unwrap());
        retry(&mut sleeper, &mut fixture.engine);
    }
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(400), "{elapsed:?}");
    let messages = received(&mut sleeper, &mut sleeper_client);
    assert_eq!(rows(&messages), ["1||2|"]);
    send(
        &mut sleeper,
        &mut fixture.engine,
        &mut sleeper_client,
        "SELECT currval('sleep_numbers'), pg_sleep(-1), pg_sleep('NaN'), pg_sleep(NULL) IS NULL",
    );
    assert_eq!(sleeper.parked_wait(), None);
    assert_eq!(
        rows(&received(&mut sleeper, &mut sleeper_client)),
        ["2|||t"]
    );
}

#[test]
fn statement_timeout_and_cancellation_end_a_sleep() {
    let mut fixture = fixture("timeout");
    let (mut sleeper, mut sleeper_client) = connection(&mut fixture, 1);
    send(
        &mut sleeper,
        &mut fixture.engine,
        &mut sleeper_client,
        "SET statement_timeout = 100",
    );
    received(&mut sleeper, &mut sleeper_client);
    let started = Instant::now();
    send(
        &mut sleeper,
        &mut fixture.engine,
        &mut sleeper_client,
        "SELECT pg_sleep(30)",
    );
    while sleeper.parked_wait().is_some() {
        let remaining = sleeper.parked_wait_remaining().unwrap();
        assert!(remaining <= Duration::from_millis(100));
        std::thread::sleep(remaining);
        retry(&mut sleeper, &mut fixture.engine);
    }
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(
        sqlstates(&received(&mut sleeper, &mut sleeper_client)),
        ["57014"]
    );
    send(
        &mut sleeper,
        &mut fixture.engine,
        &mut sleeper_client,
        "RESET statement_timeout; SELECT pg_sleep_until('infinity')",
    );
    assert_eq!(
        sleeper.parked_wait(),
        Some(crate::sql::StatementWait::Timer)
    );
    assert!(sleeper.cancel_parked(&mut fixture.engine));
    assert_eq!(
        sqlstates(&received(&mut sleeper, &mut sleeper_client)),
        ["57014"]
    );
    send(
        &mut sleeper,
        &mut fixture.engine,
        &mut sleeper_client,
        "SELECT 1",
    );
    assert_eq!(rows(&received(&mut sleeper, &mut sleeper_client)), ["1"]);
}
