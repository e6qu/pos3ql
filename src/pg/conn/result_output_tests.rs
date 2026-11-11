//! Result output against real sockets: a client that stops reading never
//! stalls the worker, other connections keep running, and the stalled client
//! later receives one complete, ordered response.

use super::*;

const KEY: &[u8; 16] = b"result-output-ky";

fn frontend(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(payload.len() + 5);
    message.push(kind);
    message.extend_from_slice(&((payload.len() + 4) as i32).to_be_bytes());
    message.extend_from_slice(payload);
    message
}

fn query(sql: &str) -> Vec<u8> {
    let mut payload = sql.as_bytes().to_vec();
    payload.push(0);
    frontend(wire::FMSG_QUERY, &payload)
}

fn set_buffer(socket: &impl AsRawFd, option: libc::c_int, bytes: libc::c_int) {
    assert_eq!(
        unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                (&bytes as *const libc::c_int).cast(),
                core::mem::size_of_val(&bytes) as libc::socklen_t,
            )
        },
        0
    );
}

/// A loopback pair whose kernel buffers hold only a few kilobytes, so a
/// result of a megabyte cannot hide in them.
///
/// The client end is the accepted socket, so it inherits the listener's
/// receive buffer before the handshake advertises a window. TCP never
/// shrinks an advertised window: shrinking the buffer after connecting lets
/// Linux overrun it, drop loopback segments and recover only through
/// backed-off retransmission timeouts, stalling the transfer for seconds.
fn socket_pair() -> (TcpStream, TcpStream) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    set_buffer(&listener, libc::SO_RCVBUF, 4096);
    let server = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (client, _) = listener.accept().unwrap();
    set_buffer(&server, libc::SO_SNDBUF, 4096);
    client.set_nonblocking(true).unwrap();
    (client, server)
}

struct Fixture {
    engine: Engine,
    budget: Budget,
    config: Config,
}

impl Fixture {
    /// Rebuilds the engine after a configuration change.
    fn with_config(config: Config) -> Self {
        let mut budget = crate::sql::test_engine_budget(&config, 1 << 30);
        let engine = Engine::new(&config, &mut budget).unwrap();
        Self {
            engine,
            budget,
            config,
        }
    }
}

fn fixture_config(mode: crate::config::TransactionMode, name: &str, spool_bytes: usize) -> Config {
    let mut config = Config::default_dev();
    config.data_dir = std::env::temp_dir()
        .join(format!(
            "pos3ql-result-output-{name}-{}-{mode:?}",
            std::process::id()
        ))
        .to_string_lossy()
        .into_owned();
    config.remove_test_data_dir_on_drop();
    config.transaction_mode = mode;
    config.max_tables = 4;
    config.max_connections = 3;
    config.row_overlay_slots = 8192;
    config.result_spool_bytes = spool_bytes;
    config.result_spool_leases = 2;
    // The table fixtures commit about a megabyte in one transaction.
    config.wal_buffer_bytes = 8 << 20;
    config
}

fn fixture(mode: crate::config::TransactionMode, name: &str, spool_bytes: usize) -> Fixture {
    Fixture::with_config(fixture_config(mode, name, spool_bytes))
}

fn connection(fixture: &mut Fixture, id: i32) -> (Conn, TcpStream) {
    let (client, server) = socket_pair();
    let mut conn = Conn::new(&fixture.config, &mut fixture.budget).unwrap();
    conn.open(server, id).unwrap();
    conn.auth_database = fixture.engine.database_login("postgres");
    conn.phase = Phase::Ready;
    (conn, client)
}

fn auth() -> AuthContext {
    AuthContext {
        mode: AuthMode::Trust,
        password: String::new(),
        scram: None,
    }
}

/// One reactor turn for a readable socket: execute, then publish.
fn readable(conn: &mut Conn, engine: &mut Engine) -> After {
    let mut poll = libc::pollfd {
        fd: conn.stream().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(
        unsafe { libc::poll(&mut poll, 1, 5000) },
        1,
        "input arrives"
    );
    let auth = auth();
    let pending = crate::mem::guard::forbid_alloc(|| conn.on_readable(engine, KEY, &auth, None));
    crate::mem::guard::forbid_alloc(|| conn.finish_response(pending, None))
}

/// Reads whatever the client socket holds right now.
fn read_available(client: &mut TcpStream, into: &mut Vec<u8>) {
    let mut chunk = [0u8; 16384];
    loop {
        match client.read(&mut chunk) {
            Ok(0) => panic!("server closed the connection"),
            Ok(n) => into.extend_from_slice(&chunk[..n]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return,
            Err(error) => panic!("client read: {error}"),
        }
    }
}

/// Drives one connection's writable and wake-up events, as the reactor
/// does each turn, until the client holds `ready_count` ReadyForQuery
/// messages.
fn drain(
    conn: &mut Conn,
    engine: &mut Engine,
    client: &mut TcpStream,
    ready_count: usize,
) -> Vec<u8> {
    drain_after(conn, engine, client, ready_count, Vec::new())
}

/// Continues `drain` after bytes the client already read, which may end
/// inside a message; messages are counted from the start of the response.
fn drain_after(
    conn: &mut Conn,
    engine: &mut Engine,
    client: &mut TcpStream,
    ready_count: usize,
    mut received: Vec<u8>,
) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        read_available(client, &mut received);
        if messages(&received).iter().filter(|m| m.0 == b'Z').count() >= ready_count {
            return received;
        }
        assert!(
            Instant::now() < deadline,
            "response did not drain: received {} bytes, last kinds {:?}, parked {:?}, send {}, overflow {}, recv {}",
            received.len(),
            messages(&received)
                .iter()
                .rev()
                .take(4)
                .map(|m| m.0 as char)
                .collect::<Vec<_>>(),
            conn.parked_wait(),
            conn.send.len(),
            conn.overflow_bytes(),
            conn.recv.len()
        );
        assert_eq!(
            crate::mem::guard::forbid_alloc(|| conn.on_writable()),
            After::Continue
        );
        if let Some(pending) = crate::mem::guard::forbid_alloc(|| {
            conn.retry_parked(engine, KEY, engine.lock_generation(), false)
        }) {
            assert_eq!(
                crate::mem::guard::forbid_alloc(|| conn.finish_response(pending, None)),
                After::Continue
            );
        }
    }
}

/// Complete backend messages as (kind, payload).
fn messages(mut bytes: &[u8]) -> Vec<(u8, &[u8])> {
    let mut out = Vec::new();
    while bytes.len() >= 5 {
        let total = 1 + u32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize;
        if total > bytes.len() {
            break;
        }
        out.push((bytes[0], &bytes[5..total]));
        bytes = &bytes[total..];
    }
    out
}

fn first_text(payload: &[u8]) -> &str {
    let length = i32::from_be_bytes(payload[2..6].try_into().unwrap()) as usize;
    core::str::from_utf8(&payload[6..6 + length]).unwrap()
}

fn sqlstate_of(payload: &[u8]) -> &str {
    let mut fields = payload;
    while let Some((&kind, rest)) = fields.split_first() {
        let end = rest.iter().position(|byte| *byte == 0).unwrap();
        if kind == b'C' {
            return core::str::from_utf8(&rest[..end]).unwrap();
        }
        fields = &rest[end + 1..];
    }
    panic!("error without SQLSTATE")
}

/// A window function is not a retained shape: a large result queues in the
/// spool.
const BIG: &str = "SELECT g, repeat('x', 200), count(*) OVER () FROM generate_series(1, 5000) g";

/// Rows 1..=count in order, each exactly once.
fn assert_rows(rows: &[&(u8, &[u8])], count: usize) {
    assert_eq!(rows.len(), count);
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(first_text(row.1), (index + 1).to_string());
    }
}

#[test]
fn stalled_client_parks_on_output_while_other_sessions_progress() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut fixture = fixture(mode, "stalled", 16 << 20);
        let (mut stalled, mut stalled_client) = connection(&mut fixture, 1);
        let (mut other, mut other_client) = connection(&mut fixture, 2);
        // Two pipelined queries: the second waits until the first drains.
        let mut input = query(BIG);
        input.extend_from_slice(&query("SELECT 2"));
        stalled_client.write_all(&input).unwrap();
        assert_eq!(readable(&mut stalled, &mut fixture.engine), After::Continue);
        assert_eq!(
            stalled.parked_wait(),
            Some(crate::sql::StatementWait::ClientOutput),
            "{:?} {} {}",
            String::from_utf8_lossy(&stalled.send.readable()[..stalled.send.len().min(400)]),
            stalled.overflow_bytes(),
            stalled.recv.len()
        );
        assert!(stalled.overflow_bytes() > 0);
        assert!(stalled.wants_write());
        assert!(!stalled.wants_read());
        // A cancel has no running statement to interrupt.
        assert!(!stalled.cancel_parked(&mut fixture.engine));
        assert!(!stalled.recv.is_empty());

        other_client
            .write_all(&query(
                "CREATE TABLE progress(v int); INSERT INTO progress VALUES (7); SELECT v FROM progress",
            ))
            .unwrap();
        assert_eq!(readable(&mut other, &mut fixture.engine), After::Continue);
        let response = drain(&mut other, &mut fixture.engine, &mut other_client, 1);
        let response = messages(&response);
        assert!(response.iter().all(|message| message.0 != b'E'));
        assert!(
            response
                .iter()
                .any(|message| message.0 == b'D' && first_text(message.1) == "7")
        );
        assert!(
            stalled.parked_wait().is_some(),
            "stalled client still unread"
        );

        let response = drain(&mut stalled, &mut fixture.engine, &mut stalled_client, 2);
        let response = messages(&response);
        let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
        assert_eq!(kinds.iter().filter(|kind| **kind == b'T').count(), 2);
        let first_ready = kinds.iter().position(|kind| *kind == b'Z').unwrap();
        let rows: Vec<_> = response[..first_ready]
            .iter()
            .filter(|message| message.0 == b'D')
            .collect();
        assert_rows(&rows, 5000);
        assert_eq!(
            &kinds[first_ready - 1..],
            b"CZTDCZ",
            "the pipelined query follows the complete first result"
        );
        assert_eq!(response[first_ready - 1].1, b"SELECT 5000\0");
        assert_eq!(first_text(response[first_ready + 2].1), "2");
        assert!(stalled.parked_wait().is_none());
        assert_eq!(stalled.overflow_bytes(), 0);
    }
}

#[test]
fn exhausted_output_spool_fails_the_result_and_keeps_the_session() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut fixture = fixture(mode, "spool", 64 << 10);
        let (mut stalled, mut stalled_client) = connection(&mut fixture, 1);
        let (mut other, mut other_client) = connection(&mut fixture, 2);
        stalled_client.write_all(&query(BIG)).unwrap();
        assert_eq!(readable(&mut stalled, &mut fixture.engine), After::Continue);
        other_client.write_all(&query("SELECT 41 + 1")).unwrap();
        assert_eq!(readable(&mut other, &mut fixture.engine), After::Continue);
        let response = drain(&mut other, &mut fixture.engine, &mut other_client, 1);
        assert!(
            messages(&response)
                .iter()
                .any(|message| message.0 == b'D' && first_text(message.1) == "42")
        );

        let response = drain(&mut stalled, &mut fixture.engine, &mut stalled_client, 1);
        let response = messages(&response);
        let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
        assert_eq!(kinds[0], b'T');
        assert_eq!(&kinds[kinds.len() - 2..], b"EZ");
        let rows: Vec<_> = response
            .iter()
            .filter(|message| message.0 == b'D')
            .collect();
        assert!(!rows.is_empty() && rows.len() < 5000);
        assert_rows(&rows, rows.len());
        let error = response[kinds.len() - 2].1;
        assert_eq!(sqlstate_of(error), "54000");
        assert!(
            error
                .windows(b"result_spool_bytes".len())
                .any(|part| part == b"result_spool_bytes"),
            "{}",
            String::from_utf8_lossy(error)
        );
        assert_eq!(response[kinds.len() - 1].1, b"I");

        stalled_client.write_all(&query("SELECT 3")).unwrap();
        assert_eq!(readable(&mut stalled, &mut fixture.engine), After::Continue);
        let response = drain(&mut stalled, &mut fixture.engine, &mut stalled_client, 1);
        assert!(
            messages(&response)
                .iter()
                .any(|message| message.0 == b'D' && first_text(message.1) == "3")
        );
    }
}

#[test]
fn unpaged_extended_execute_queues_large_results_and_mid_result_errors() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut fixture = fixture(mode, "extended", 16 << 20);
        let (mut conn, mut client) = connection(&mut fixture, 1);
        for (sql, rows, error) in [
            (BIG, 5000usize, None),
            (
                "SELECT g, 1 / (g - 3001) FROM generate_series(1, 5000) g, (VALUES (1)) v",
                3000,
                Some("22012"),
            ),
        ] {
            let mut parse = b"\0".to_vec();
            parse.extend_from_slice(sql.as_bytes());
            parse.extend_from_slice(b"\0\0\0");
            let mut input = frontend(wire::FMSG_PARSE, &parse);
            input.extend_from_slice(&frontend(wire::FMSG_BIND, b"\0\0\0\0\0\0\0\0"));
            input.extend_from_slice(&frontend(wire::FMSG_EXECUTE, b"\0\0\0\0\0"));
            input.extend_from_slice(&frontend(wire::FMSG_SYNC, b""));
            client.write_all(&input).unwrap();
            assert_eq!(readable(&mut conn, &mut fixture.engine), After::Continue);
            let response = drain(&mut conn, &mut fixture.engine, &mut client, 1);
            let response = messages(&response);
            let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
            assert_eq!(&kinds[..2], b"12");
            let data: Vec<_> = response
                .iter()
                .filter(|message| message.0 == b'D')
                .collect();
            match error {
                None => {
                    assert_rows(&data, rows);
                    assert_eq!(&kinds[kinds.len() - 2..], b"CZ");
                    assert_eq!(response[kinds.len() - 2].1, b"SELECT 5000\0");
                }
                Some(code) => {
                    // Rows before the failure may or may not have been sent;
                    // the ones that were are complete and in order.
                    assert!(data.len() <= rows);
                    assert_rows(&data, data.len());
                    assert_eq!(&kinds[kinds.len() - 2..], b"EZ");
                    assert_eq!(sqlstate_of(response[kinds.len() - 2].1), code);
                }
            }
        }
    }
}

#[test]
fn notifications_wait_for_the_result_in_progress() {
    let mut fixture = fixture(crate::config::TransactionMode::Mvcc, "notify", 16 << 20);
    let (mut conn, mut client) = connection(&mut fixture, 1);
    client.write_all(&query(BIG)).unwrap();
    assert_eq!(readable(&mut conn, &mut fixture.engine), After::Continue);
    assert!(conn.parked_wait().is_some());
    assert!(conn.queue_notification(9, "jobs", "ready"));
    let response = drain(&mut conn, &mut fixture.engine, &mut client, 1);
    let mut response = response;
    let deadline = crate::test_wait::deadline();
    while !messages(&response).iter().any(|message| message.0 == b'A') {
        assert!(
            Instant::now() < deadline,
            "held notification was not delivered"
        );
        assert_eq!(conn.on_writable(), After::Continue);
        read_available(&mut client, &mut response);
    }
    let kinds: Vec<u8> = messages(&response)
        .iter()
        .map(|message| message.0)
        .collect();
    assert_eq!(&kinds[kinds.len() - 3..], b"CZA");
}

/// Completes a TLS handshake between `conn` and a rustls client on `socket`
/// and installs the server session on the connection.
fn tls_client(conn: &mut Conn, socket: &mut TcpStream) -> rustls::ClientConnection {
    let server_config = crate::pg::tls::build_server_config(
        "tests/data/tls-test-cert.pem",
        "tests/data/tls-test-key.pem",
    )
    .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    let pem = std::fs::read_to_string("tests/data/tls-test-cert.pem").unwrap();
    for certificate in crate::pem::certificates(&pem).unwrap() {
        roots
            .add(rustls::pki_types::CertificateDer::from(certificate))
            .unwrap();
    }
    let client_config = std::sync::Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let mut session = crate::pg::tls::ServerSession::new(&server_config).unwrap();
    let mut client = rustls::ClientConnection::new(
        client_config,
        rustls::pki_types::ServerName::try_from("localhost").unwrap(),
    )
    .unwrap();
    let deadline = crate::test_wait::deadline();
    while client.is_handshaking() || session.is_handshaking() {
        assert!(Instant::now() < deadline, "handshake stalled");
        while client.wants_write() {
            match client.write_tls(socket) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("client write: {error}"),
            }
        }
        let mut scratch = [0u8; 64];
        let _ = session.read(conn.stream.as_mut().unwrap(), &mut scratch);
        session
            .flush_nonblocking(conn.stream.as_mut().unwrap())
            .unwrap();
        match client.read_tls(socket) {
            Ok(_) => {
                client.process_new_packets().unwrap();
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("client read: {error}"),
        }
    }
    conn.tls = Some(session);
    client
}

/// Reads whatever plaintext the TLS client can decrypt right now.
fn read_tls_available(
    client: &mut rustls::ClientConnection,
    socket: &mut TcpStream,
    into: &mut Vec<u8>,
) {
    loop {
        match client.read_tls(socket) {
            Ok(0) => panic!("server closed the TLS connection"),
            Ok(_) => {
                client.process_new_packets().unwrap();
                let mut chunk = [0u8; 16384];
                loop {
                    match client.reader().read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => into.extend_from_slice(&chunk[..n]),
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(error) => panic!("client plaintext: {error}"),
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return,
            Err(error) => panic!("client TLS read: {error}"),
        }
    }
}

#[test]
fn tls_backpressure_keeps_ciphertext_queued_instead_of_closing() {
    use crate::mem::lease_pool::{LeasePool, Purpose};
    let (mut socket, server) = socket_pair();
    let config = Config::default_dev();
    let mut budget = Budget::new(256 << 20);
    let mut conn = Conn::new(&config, &mut budget).unwrap();
    conn.open(server, 5).unwrap();
    let mut client = tls_client(&mut conn, &mut socket);
    // Queue far more than the kernel and the 64 KiB rustls record buffer
    // hold while the client reads nothing.
    let spool = LeasePool::new(&mut budget, Purpose::RESULT_SPOOL, 1 << 20, 1).unwrap();
    let row = crate::sql::types::Datum::Text("0123456789abcdef0123456789abcdef");
    let rows = 12_000usize;
    {
        let mut responder = Responder::new(&mut conn.send).with_output(OutputSink::new(
            &mut conn.overflow,
            Some(spool.clone()),
            ClientWriter::None,
        ));
        for _ in 0..rows {
            responder.data_row(core::slice::from_ref(&row)).unwrap();
        }
    }
    for _ in 0..4 {
        assert_eq!(conn.on_writable(), After::Continue);
    }
    assert!(conn.wants_write());
    let mut plaintext = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while plaintext.len() < rows * 43 {
        assert!(Instant::now() < deadline, "TLS output did not drain");
        assert_eq!(conn.on_writable(), After::Continue);
        read_tls_available(&mut client, &mut socket, &mut plaintext);
    }
    assert_eq!(plaintext.len(), rows * 43);
    assert!(messages(&plaintext).iter().all(|message| message.0 == b'D'));
    drop(conn);
}

/// Runs `sql` to completion on an idle connection.
fn run(conn: &mut Conn, engine: &mut Engine, client: &mut TcpStream, sql: &str) -> Vec<u8> {
    client.write_all(&query(sql)).unwrap();
    assert_eq!(readable(conn, engine), After::Continue);
    drain(conn, engine, client, 1)
}

fn stream_fixture(
    mode: crate::config::TransactionMode,
    name: &str,
) -> (Fixture, Conn, TcpStream, Conn, TcpStream) {
    let mut config = fixture_config(mode, name, 16 << 20);
    config.result_stream_bytes = 1 << 20;
    let mut fixture = Fixture::with_config(config);
    let (mut reader, mut reader_client) = connection(&mut fixture, 1);
    let (other, other_client) = connection(&mut fixture, 2);
    let response = run(
        &mut reader,
        &mut fixture.engine,
        &mut reader_client,
        "CREATE TABLE big(id int, payload text); \
         INSERT INTO big SELECT g, repeat('x', 200) FROM generate_series(1, 5000) g",
    );
    assert!(
        messages(&response).iter().all(|message| message.0 != b'E'),
        "{}",
        String::from_utf8_lossy(&response)
    );
    (fixture, reader, reader_client, other, other_client)
}

#[test]
fn retained_selects_resume_as_streams_while_other_sessions_progress() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let (mut fixture, mut stalled, mut stalled_client, mut other, mut other_client) =
            stream_fixture(mode, "stream");
        let opened = fixture.engine.result_stream_metrics().opened;
        let mut input = query("SELECT id, payload FROM big");
        input.extend_from_slice(&query("SELECT 2"));
        stalled_client.write_all(&input).unwrap();
        assert_eq!(readable(&mut stalled, &mut fixture.engine), After::Continue);
        assert_eq!(
            stalled.parked_wait(),
            Some(crate::sql::StatementWait::ClientOutput)
        );
        assert!(stalled.result_streams.is_active());
        // A stream is not an SQL cursor: pg_cursors and max_cursors ignore it.
        assert_eq!(stalled.cursors.len(), 0);
        assert_eq!(fixture.engine.result_stream_metrics().opened, opened + 1);
        // A stream keeps at most one row beyond the send buffer.
        assert!(stalled.overflow_bytes() < 1024);
        // Locking mode makes a writer of the streamed table wait for the
        // reader's transaction, as its readers' locks require.
        let progress = if mode == crate::config::TransactionMode::Mvcc {
            "INSERT INTO big VALUES (9001, 'y'); SELECT count(*) FROM big"
        } else {
            "CREATE TABLE progress(v int); INSERT INTO progress VALUES (1); \
             SELECT count(*) FROM progress"
        };
        let response = run(&mut other, &mut fixture.engine, &mut other_client, progress);
        assert!(messages(&response).iter().all(|message| message.0 != b'E'));
        assert!(stalled.result_streams.is_active());

        let response = drain(&mut stalled, &mut fixture.engine, &mut stalled_client, 2);
        let response = messages(&response);
        let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
        let first_ready = kinds.iter().position(|kind| *kind == b'Z').unwrap();
        assert_eq!(kinds[0], b'T');
        let rows: Vec<_> = response[..first_ready]
            .iter()
            .filter(|message| message.0 == b'D')
            .collect();
        assert_rows(&rows, 5000);
        assert_eq!(&kinds[first_ready - 1..], b"CZTDCZ");
        assert_eq!(
            kinds[..first_ready]
                .iter()
                .filter(|kind| **kind == b'T')
                .count(),
            1
        );
        assert_eq!(response[first_ready - 1].1, b"SELECT 5000\0");
        assert!(!stalled.result_streams.is_active());
        assert!(stalled.parked_wait().is_none());
    }
}

#[test]
fn joins_stream_rows_as_the_client_reads() {
    let (mut fixture, mut conn, mut client, mut other, mut other_client) =
        stream_fixture(crate::config::TransactionMode::Mvcc, "stream-joins");
    run(
        &mut other,
        &mut fixture.engine,
        &mut other_client,
        "CREATE TABLE tags(k int, label text); \
         INSERT INTO tags VALUES (0, 'zero'), (1, 'one'), (2, NULL), (3, 'three')",
    );
    // Each joins every row of big to at most one tag; neither condition is a
    // column equality, so the general executor also reads a nested loop.
    for (sql, expected) in [
        (
            "SELECT b.id, b.payload, t.label FROM big b JOIN tags t ON t.k = b.id % 4",
            5000,
        ),
        (
            "SELECT b.id, b.payload, t.label FROM big b LEFT JOIN tags t \
             ON t.k = b.id % 8 AND t.label IS NOT NULL",
            5000,
        ),
        (
            "SELECT b.id, b.payload, g FROM big b CROSS JOIN generate_series(1, 2) g \
             WHERE g <= 1",
            5000,
        ),
        (
            "SELECT b.id, b.payload, v.x FROM big b, (VALUES (1), (2)) v(x) \
             WHERE v.x = b.id % 2 + 1",
            5000,
        ),
        // Column equalities: the general executor hashes tags, and the
        // stream builds the same hash table in plan_build_bytes.
        (
            "SELECT b.id, b.payload, t.label FROM big b LEFT JOIN tags t ON t.k = b.id",
            5000,
        ),
        (
            "SELECT b.id, b.payload, c.id FROM big b, big c WHERE c.id = b.id",
            5000,
        ),
    ] {
        let opened = fixture.engine.result_stream_metrics().opened;
        client.write_all(&query(sql)).unwrap();
        assert_eq!(readable(&mut conn, &mut fixture.engine), After::Continue);
        assert!(conn.result_streams.is_active(), "{sql}");
        assert!(conn.overflow_bytes() < 1024, "{sql}");
        // DDL on a joined table waits for the stream's holds.
        let response = drain(&mut conn, &mut fixture.engine, &mut client, 1);
        let response = messages(&response);
        let mut ids: Vec<i64> = response
            .iter()
            .filter(|message| message.0 == b'D')
            .map(|message| first_text(message.1).parse().unwrap())
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, (1..=expected).collect::<Vec<i64>>(), "{sql}");
        assert_eq!(
            response[response.len() - 2].1,
            format!("SELECT {expected}\0").as_bytes(),
            "{sql}"
        );
        assert_eq!(
            fixture.engine.result_stream_metrics().opened,
            opened + 1,
            "{sql}"
        );
        // The same statement through the general executor returns the same
        // rows: compare every column.
        let mut streamed: Vec<Vec<Option<Vec<u8>>>> = response
            .iter()
            .filter(|message| message.0 == b'D')
            .map(|message| row_values(message.1))
            .collect();
        streamed.sort();
        let general = run(
            &mut other,
            &mut fixture.engine,
            &mut other_client,
            &format!("SELECT * FROM ({sql}) q ORDER BY 1"),
        );
        let general: Vec<Vec<Option<Vec<u8>>>> = messages(&general)
            .iter()
            .filter(|message| message.0 == b'D')
            .map(|message| row_values(message.1))
            .collect();
        assert_eq!(streamed.len(), general.len(), "{sql}");
        let mut general = general;
        general.sort();
        assert_eq!(streamed, general, "{sql}");
    }
}

fn row_values(payload: &[u8]) -> Vec<Option<Vec<u8>>> {
    let count = u16::from_be_bytes(payload[..2].try_into().unwrap()) as usize;
    let mut at = 2;
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        let length = i32::from_be_bytes(payload[at..at + 4].try_into().unwrap());
        at += 4;
        if length < 0 {
            values.push(None);
        } else {
            values.push(Some(payload[at..at + length as usize].to_vec()));
            at += length as usize;
        }
    }
    values
}

#[test]
fn sorted_results_stream_rows_as_the_client_reads() {
    let (mut fixture, mut conn, mut client, mut other, mut other_client) =
        stream_fixture(crate::config::TransactionMode::Mvcc, "stream-sorts");
    run(
        &mut other,
        &mut fixture.engine,
        &mut other_client,
        "CREATE TABLE tags(k int, label text); \
         INSERT INTO tags VALUES (0, 'zero'), (1, 'one'), (2, NULL), (3, 'three')",
    );
    for (sql, expected) in [
        ("SELECT id, payload FROM big ORDER BY id DESC", 5000),
        (
            "SELECT b.id, b.payload, t.label FROM big b LEFT JOIN tags t ON t.k = b.id % 4 \
             ORDER BY t.label NULLS FIRST, b.id DESC",
            5000,
        ),
        // With LIMIT, output that PostgreSQL could postpone past the sort
        // must be a column or constant.
        (
            &*format!(
                "SELECT g, '{}' FROM generate_series(1, 6000) g \
                 WHERE g % 3 <> 0 ORDER BY g % 7, g LIMIT 3000 OFFSET 5",
                "x".repeat(200)
            ),
            3000,
        ),
    ] {
        let opened = fixture.engine.result_stream_metrics().opened;
        client.write_all(&query(sql)).unwrap();
        assert_eq!(readable(&mut conn, &mut fixture.engine), After::Continue);
        assert!(conn.result_streams.is_active(), "{sql}");
        assert!(conn.overflow_bytes() < 1024, "{sql}");
        let response = drain(&mut conn, &mut fixture.engine, &mut client, 1);
        let response = messages(&response);
        let streamed: Vec<Vec<Option<Vec<u8>>>> = response
            .iter()
            .filter(|message| message.0 == b'D')
            .map(|message| row_values(message.1))
            .collect();
        assert_eq!(streamed.len(), expected, "{sql}");
        assert_eq!(
            fixture.engine.result_stream_metrics().opened,
            opened + 1,
            "{sql}"
        );
        // The general executor returns the same rows in the same order.
        let general = run(
            &mut other,
            &mut fixture.engine,
            &mut other_client,
            &format!("SELECT * FROM ({sql}) q"),
        );
        let general: Vec<Vec<Option<Vec<u8>>>> = messages(&general)
            .iter()
            .filter(|message| message.0 == b'D')
            .map(|message| row_values(message.1))
            .collect();
        assert!(streamed == general, "{sql}");
    }
}

/// Rows of a response as text values, in order.
fn data_rows(response: &[(u8, &[u8])]) -> Vec<Vec<Option<Vec<u8>>>> {
    response
        .iter()
        .filter(|message| message.0 == b'D')
        .map(|message| row_values(message.1))
        .collect()
}

#[test]
fn builds_that_outgrow_plan_build_bytes_spool_instead() {
    // A stream's sort or hash table lives in plan_build_bytes; the general
    // executor builds the same rows in statement memory. A promoted result
    // whose build does not fit runs again spooled, before anything of the
    // stream is sent, so promotion never fails a result that spools.
    let mut config = fixture_config(
        crate::config::TransactionMode::Mvcc,
        "stream-build-capacity",
        16 << 20,
    );
    config.result_stream_bytes = 1 << 20;
    config.plan_build_bytes = 64 << 10;
    config.portal_result_bytes = 2 << 20;
    let mut fixture = Fixture::with_config(config);
    let (mut conn, mut client) = connection(&mut fixture, 1);
    let (mut other, mut other_client) = connection(&mut fixture, 2);
    let setup = run(
        &mut conn,
        &mut fixture.engine,
        &mut client,
        "CREATE TABLE big(id int, payload text); \
         INSERT INTO big SELECT g, repeat('x', 200) FROM generate_series(1, 5000) g; \
         CREATE FUNCTION same(int) RETURNS int LANGUAGE sql AS 'SELECT $1'",
    );
    assert!(messages(&setup).iter().all(|message| message.0 != b'E'));
    let sorted = "SELECT g, repeat('x', 200) FROM generate_series(1, 5000) g ORDER BY g DESC";
    let joined = "SELECT b.id, b.payload, c.id FROM big b JOIN big c ON c.id = b.id";
    // A routine call in the output runs the promotion attempt itself through
    // a retained reader, which also falls back before describing its result.
    // The general executor restarts at each first call, so keep it short.
    let handed_off = "SELECT same(b.id), repeat(b.payload, 10), c.id FROM big b \
                      JOIN big c ON c.id = b.id WHERE b.id <= 100";
    for (sql, count) in [(sorted, 5000), (joined, 5000), (handed_off, 100)] {
        let before = fixture.engine.result_stream_metrics();
        let response = run(&mut conn, &mut fixture.engine, &mut client, sql);
        let response = messages(&response);
        let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
        assert_eq!(
            kinds[0],
            b'T',
            "{sql}: {}",
            String::from_utf8_lossy(response[0].1)
        );
        assert_eq!(&kinds[kinds.len() - 2..], b"CZ", "{sql}");
        assert_eq!(
            response[kinds.len() - 2].1,
            format!("SELECT {count}\0").as_bytes(),
            "{sql}"
        );
        let mut rows = data_rows(&response);
        let general = run(
            &mut other,
            &mut fixture.engine,
            &mut other_client,
            &format!("SELECT * FROM ({sql}) q"),
        );
        let mut general = data_rows(&messages(&general));
        if sql != sorted {
            rows.sort();
            general.sort();
        }
        assert_eq!(rows.len(), count, "{sql}");
        assert!(rows == general, "{sql}");
        let after = fixture.engine.result_stream_metrics();
        assert_eq!(after.opened, before.opened, "{sql}");
        if sql == handed_off {
            // Its routine call starts an effect before the output overflows.
            assert_eq!(after.spooled_effects, before.spooled_effects + 1, "{sql}");
        } else {
            assert_eq!(after.spooled_capacity, before.spooled_capacity + 1, "{sql}");
        }
        assert!(!conn.result_streams.is_held());
    }

    // An unpaged Execute, a paged Execute and COPY run again spooled too.
    let before = fixture.engine.result_stream_metrics();
    let mut input = parse("", sorted);
    input.extend_from_slice(&bind("", ""));
    input.extend_from_slice(&execute("", 0));
    input.extend_from_slice(&parse("", joined));
    input.extend_from_slice(&bind("", ""));
    input.extend_from_slice(&execute("", 10));
    input.extend_from_slice(&execute("", 0));
    input.extend_from_slice(&sync());
    client.write_all(&input).unwrap();
    assert_eq!(readable(&mut conn, &mut fixture.engine), After::Continue);
    let response = drain(&mut conn, &mut fixture.engine, &mut client, 1);
    let response = messages(&response);
    let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
    assert!(!kinds.contains(&b'E'), "{kinds:?}");
    let tags: Vec<&[u8]> = response
        .iter()
        .filter(|message| message.0 == b'C')
        .map(|message| message.1)
        .collect();
    assert_eq!(tags, [&b"SELECT 5000\0"[..], b"SELECT 4990\0"]);
    assert_eq!(kinds.iter().filter(|kind| **kind == b's').count(), 1);
    let rows = data_rows(&response);
    assert_eq!(rows.len(), 10000);
    assert_eq!(rows[0][0].as_deref(), Some(&b"5000"[..]));
    assert_eq!(rows[4999][0].as_deref(), Some(&b"1"[..]));
    let after = fixture.engine.result_stream_metrics();
    assert_eq!(after.opened, before.opened);
    assert_eq!(after.spooled_capacity, before.spooled_capacity + 2);

    let before = fixture.engine.result_stream_metrics();
    let response = run(
        &mut conn,
        &mut fixture.engine,
        &mut client,
        &format!("COPY ({sorted}) TO STDOUT"),
    );
    let response = messages(&response);
    let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
    assert_eq!(kinds[0], b'H');
    assert_eq!(&kinds[kinds.len() - 3..], b"cCZ");
    assert_eq!(response[kinds.len() - 2].1, b"COPY 5000\0");
    assert_eq!(kinds.iter().filter(|kind| **kind == b'd').count(), 5000);
    assert!(response[1].1.starts_with(b"5000\t"));
    let after = fixture.engine.result_stream_metrics();
    assert_eq!(after.opened, before.opened);
    assert_eq!(after.spooled_capacity, before.spooled_capacity + 1);

    // A declared cursor builds its hash table at DECLARE; one that does not
    // fit saves its rows there instead, as it did before joins streamed.
    let response = run(
        &mut conn,
        &mut fixture.engine,
        &mut client,
        "BEGIN; DECLARE c CURSOR FOR SELECT b.id, c.id FROM big b JOIN big c \
         ON c.id = b.id WHERE b.id <= 100; FETCH 10 c; FETCH ALL c; COMMIT",
    );
    let response = messages(&response);
    assert!(response.iter().all(|message| message.0 != b'E'));
    let tags: Vec<&[u8]> = response
        .iter()
        .filter(|message| message.0 == b'C')
        .map(|message| message.1)
        .collect();
    assert_eq!(
        tags,
        [
            &b"BEGIN\0"[..],
            b"DECLARE CURSOR\0",
            b"FETCH 10\0",
            b"FETCH 90\0",
            b"COMMIT\0"
        ]
    );
    let mut ids: Vec<i64> = data_rows(&response)
        .iter()
        .map(|row| {
            core::str::from_utf8(row[0].as_deref().unwrap())
                .unwrap()
                .parse()
                .unwrap()
        })
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, (1..=100).collect::<Vec<i64>>());
}

/// Writes a request larger than the loopback buffers, letting the
/// connection read as the socket fills; the caller processes the rest.
fn send_large(conn: &mut Conn, engine: &mut Engine, client: &mut TcpStream, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        match client.write(bytes) {
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert_eq!(readable(conn, engine), After::Continue);
            }
            Err(error) => panic!("client write: {error}"),
        }
    }
}

#[test]
fn function_sources_stream_rows_as_the_client_reads() {
    let (mut fixture, mut conn, mut client, _, _) =
        stream_fixture(crate::config::TransactionMode::Mvcc, "stream-functions");
    // Short rows from a constant that fits the receive buffer still outgrow
    // the send buffer several times over.
    let pieces: Vec<String> = (0..12_000).map(|row| (row % 10).to_string()).collect();
    let split = pieces.join(",");
    let elements = format!("[\"{}\"]", pieces.join("\",\""));
    for sql in [
        format!("SELECT piece FROM regexp_split_to_table('{split}', ',') piece"),
        format!("SELECT value FROM json_array_elements_text('{elements}')"),
        format!(
            "SELECT * FROM ROWS FROM (regexp_split_to_table('{split}', ','), \
             generate_series(1, 2)) WITH ORDINALITY"
        ),
    ] {
        let opened = fixture.engine.result_stream_metrics().opened;
        send_large(&mut conn, &mut fixture.engine, &mut client, &query(&sql));
        // Loopback may still be delivering the request's tail.
        for _ in 0..50 {
            assert_eq!(readable(&mut conn, &mut fixture.engine), After::Continue);
            if conn.result_streams.is_active() {
                break;
            }
        }
        assert!(conn.result_streams.is_active(), "{sql:.80}");
        // The stream holds the call's position, not its rows.
        assert!(conn.overflow_bytes() < 1024);
        let response = drain(&mut conn, &mut fixture.engine, &mut client, 1);
        let response = messages(&response);
        let rows: Vec<_> = response
            .iter()
            .filter(|message| message.0 == b'D')
            .collect();
        assert_eq!(rows.len(), pieces.len(), "{sql:.80}");
        for (row, expected) in rows.iter().zip(&pieces) {
            assert_eq!(first_text(row.1), expected);
        }
        assert_eq!(response[response.len() - 2].1, b"SELECT 12000\0");
        assert_eq!(fixture.engine.result_stream_metrics().opened, opened + 1);
    }
}

#[test]
fn result_streams_apply_bounds_and_fail_after_delivered_rows() {
    let (mut fixture, mut conn, mut client, _, _) =
        stream_fixture(crate::config::TransactionMode::Mvcc, "stream-bounds");
    client
        .write_all(&query(
            "SELECT id, payload FROM big WHERE id > 1000 OFFSET 10 LIMIT 3000",
        ))
        .unwrap();
    assert_eq!(readable(&mut conn, &mut fixture.engine), After::Continue);
    assert!(conn.result_streams.is_active());
    let response = drain(&mut conn, &mut fixture.engine, &mut client, 1);
    let response = messages(&response);
    let rows: Vec<_> = response
        .iter()
        .filter(|message| message.0 == b'D')
        .collect();
    assert_eq!(rows.len(), 3000);
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(first_text(row.1), (1011 + index).to_string());
    }
    assert_eq!(response[response.len() - 2].1, b"SELECT 3000\0");

    client
        .write_all(&query("SELECT id, 1 / (id - 3001), payload FROM big"))
        .unwrap();
    assert_eq!(readable(&mut conn, &mut fixture.engine), After::Continue);
    assert!(conn.result_streams.is_active());
    let response = drain(&mut conn, &mut fixture.engine, &mut client, 1);
    let response = messages(&response);
    let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
    let rows: Vec<_> = response
        .iter()
        .filter(|message| message.0 == b'D')
        .collect();
    // Delivered rows are final: every row before the failure arrives.
    assert_rows(&rows, 3000);
    assert_eq!(&kinds[kinds.len() - 2..], b"EZ");
    assert_eq!(sqlstate_of(response[kinds.len() - 2].1), "22012");
    assert_eq!(response[kinds.len() - 1].1, b"I");
    assert!(!conn.result_streams.is_active());
}

#[test]
fn result_streams_end_on_cancel_timeout_and_disconnect() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let (mut fixture, mut conn, mut client, mut other, mut other_client) =
            stream_fixture(mode, "stream-cleanup");
        // Cancel: rows already queued, then 57014 and ReadyForQuery.
        client
            .write_all(&query("SELECT id, payload FROM big"))
            .unwrap();
        assert_eq!(readable(&mut conn, &mut fixture.engine), After::Continue);
        assert!(conn.result_streams.is_active());
        assert!(crate::mem::guard::forbid_alloc(|| {
            conn.cancel_parked(&mut fixture.engine)
        }));
        assert!(!conn.result_streams.is_active());
        let response = drain(&mut conn, &mut fixture.engine, &mut client, 1);
        let response = messages(&response);
        let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
        assert_eq!(&kinds[kinds.len() - 2..], b"EZ");
        assert_eq!(sqlstate_of(response[kinds.len() - 2].1), "57014");
        let rows: Vec<_> = response
            .iter()
            .filter(|message| message.0 == b'D')
            .collect();
        assert_rows(&rows, rows.len());

        // statement_timeout expires while the stream waits for its client.
        // The armed deadline is moved into the past rather than slept past,
        // so a loaded host cannot expire it before the stream opens.
        run(
            &mut conn,
            &mut fixture.engine,
            &mut client,
            "SET statement_timeout = '1h'",
        );
        let submitted = crate::sql::datetime::now_micros();
        client
            .write_all(&query("SELECT id, payload FROM big"))
            .unwrap();
        assert_eq!(readable(&mut conn, &mut fixture.engine), After::Continue);
        assert!(conn.result_streams.is_active());
        let armed = conn.expire_result_stream_deadline();
        assert!(
            (submitted + 3_600_000_000..=crate::sql::datetime::now_micros() + 3_600_000_000)
                .contains(&armed),
            "the stream keeps the statement's deadline"
        );
        let generation = fixture.engine.lock_generation();
        let pending = crate::mem::guard::forbid_alloc(|| {
            conn.retry_parked(&mut fixture.engine, KEY, generation, false)
        })
        .expect("the deadline wakes the stream");
        conn.finish_response(pending, None);
        assert!(!conn.result_streams.is_active());
        let response = drain(&mut conn, &mut fixture.engine, &mut client, 1);
        let response = messages(&response);
        let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
        assert_eq!(&kinds[kinds.len() - 2..], b"EZ");
        assert_eq!(sqlstate_of(response[kinds.len() - 2].1), "57014");
        run(
            &mut conn,
            &mut fixture.engine,
            &mut client,
            "SET statement_timeout = 0",
        );

        // Disconnect: closing the connection releases the stream's readers
        // and, with its transaction, the table lock.
        client
            .write_all(&query("SELECT id, payload FROM big"))
            .unwrap();
        assert_eq!(readable(&mut conn, &mut fixture.engine), After::Continue);
        assert!(conn.result_streams.is_active());
        fixture.engine.rollback_txn(&mut conn.txn, &conn.guc);
        drop(conn.close());
        assert!(!conn.result_streams.is_active());
        let response = run(
            &mut other,
            &mut fixture.engine,
            &mut other_client,
            "DROP TABLE big",
        );
        assert!(
            messages(&response).iter().all(|message| message.0 != b'E'),
            "{:?}",
            String::from_utf8_lossy(&response)
        );
        assert!(other.parked_wait().is_none());
    }
}

#[test]
fn unpaged_extended_execute_resumes_retained_results() {
    let (mut fixture, mut conn, mut client, _, _) =
        stream_fixture(crate::config::TransactionMode::Mvcc, "stream-extended");
    for (sql, rows, error) in [
        ("SELECT id, payload FROM big", 5000usize, None),
        (
            "SELECT id, 1 / (id - 4001), payload FROM big",
            4000,
            Some("22012"),
        ),
    ] {
        let mut parse = b"\0".to_vec();
        parse.extend_from_slice(sql.as_bytes());
        parse.extend_from_slice(b"\0\0\0");
        let mut input = frontend(wire::FMSG_PARSE, &parse);
        input.extend_from_slice(&frontend(wire::FMSG_BIND, b"\0\0\0\0\0\0\0\0"));
        input.extend_from_slice(&frontend(wire::FMSG_EXECUTE, b"\0\0\0\0\0"));
        input.extend_from_slice(&frontend(wire::FMSG_SYNC, b""));
        client.write_all(&input).unwrap();
        assert_eq!(readable(&mut conn, &mut fixture.engine), After::Continue);
        assert!(conn.result_streams.is_active());
        let response = drain(&mut conn, &mut fixture.engine, &mut client, 1);
        let response = messages(&response);
        let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
        assert_eq!(&kinds[..2], b"12");
        assert!(
            !kinds.contains(&b'T'),
            "Execute does not resend RowDescription"
        );
        let data: Vec<_> = response
            .iter()
            .filter(|message| message.0 == b'D')
            .collect();
        assert_rows(&data, rows);
        match error {
            None => {
                assert_eq!(&kinds[kinds.len() - 2..], b"CZ");
                assert_eq!(response[kinds.len() - 2].1, b"SELECT 5000\0");
            }
            Some(code) => {
                assert_eq!(&kinds[kinds.len() - 2..], b"EZ");
                assert_eq!(sqlstate_of(response[kinds.len() - 2].1), code);
            }
        }
        assert!(!conn.result_streams.is_active());
    }
}

#[test]
fn spooled_results_record_why_they_did_not_stream() {
    let (mut fixture, mut conn, mut client, _, _) =
        stream_fixture(crate::config::TransactionMode::Mvcc, "stream-reasons");
    let before = fixture.engine.result_stream_metrics();
    run(
        &mut conn,
        &mut fixture.engine,
        &mut client,
        "CREATE SEQUENCE numbers",
    );
    for sql in [
        "SELECT id, payload, count(*) OVER () FROM big",
        "SELECT id, nextval('numbers'), payload FROM big",
    ] {
        let response = run(&mut conn, &mut fixture.engine, &mut client, sql);
        let rows = messages(&response)
            .iter()
            .filter(|message| message.0 == b'D')
            .count();
        assert_eq!(rows, 5000, "{sql}");
        assert!(!conn.result_streams.is_active());
    }
    let after = fixture.engine.result_stream_metrics();
    assert_eq!(after.opened, before.opened);
    assert_eq!(after.spooled_shape, before.spooled_shape + 1);
    assert_eq!(after.spooled_effects, before.spooled_effects + 1);

    // A capture estimate above result_stream_bytes chooses the spool before
    // the statement runs.
    drop((conn, fixture));
    let mut small = fixture_config(
        crate::config::TransactionMode::Mvcc,
        "stream-small",
        16 << 20,
    );
    small.result_stream_bytes = 64 << 10;
    let mut fixture = Fixture::with_config(small);
    let (mut small_conn, mut small_client) = connection(&mut fixture, 7);
    let engine = &mut fixture.engine;
    run(
        &mut small_conn,
        engine,
        &mut small_client,
        "CREATE TABLE wide(id int, payload text); \
         INSERT INTO wide SELECT g, repeat('x', 200) FROM generate_series(1, 5000) g",
    );
    let response = run(
        &mut small_conn,
        engine,
        &mut small_client,
        "SELECT id, payload FROM wide",
    );
    assert_eq!(
        messages(&response)
            .iter()
            .filter(|message| message.0 == b'D')
            .count(),
        5000
    );
    let metrics = engine.result_stream_metrics();
    assert_eq!(metrics.opened, 0);
    assert_eq!(metrics.spooled_capacity, 1);
}

#[test]
fn stream_rows_take_their_effects_once_across_client_waits() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let (mut fixture, mut conn, mut client, _, _) = stream_fixture(mode, "stream-effects");
        run(
            &mut conn,
            &mut fixture.engine,
            &mut client,
            "CREATE SEQUENCE numbers",
        );
        // The first rows overflow the send buffer before any row calls
        // nextval, so the statement promotes; later rows each take a number
        // while the stream repeatedly waits for its client.
        client
            .write_all(&query(
                "SELECT id, CASE WHEN id > 1000 THEN nextval('numbers') END, payload FROM big",
            ))
            .unwrap();
        assert_eq!(readable(&mut conn, &mut fixture.engine), After::Continue);
        assert!(conn.result_streams.is_active());
        let response = drain(&mut conn, &mut fixture.engine, &mut client, 1);
        let response = messages(&response);
        let rows: Vec<_> = response
            .iter()
            .filter(|message| message.0 == b'D')
            .collect();
        assert_rows(&rows, 5000);
        for (index, row) in rows.iter().enumerate().skip(1000) {
            let payload = row.1;
            let first = 6 + i32::from_be_bytes(payload[2..6].try_into().unwrap()) as usize;
            let length = i32::from_be_bytes(payload[first..first + 4].try_into().unwrap()) as usize;
            let value = core::str::from_utf8(&payload[first + 4..first + 4 + length]).unwrap();
            assert_eq!(value, (index - 999).to_string());
        }
        let response = run(
            &mut conn,
            &mut fixture.engine,
            &mut client,
            "SELECT nextval('numbers')",
        );
        assert!(
            messages(&response)
                .iter()
                .any(|message| message.0 == b'D' && first_text(message.1) == "4001")
        );
    }
}

#[test]
fn stream_rows_wait_for_locks_without_repeating_delivered_rows_or_effects() {
    let (mut fixture, mut conn, mut client, mut holder, mut holder_client) =
        stream_fixture(crate::config::TransactionMode::Mvcc, "stream-lock");
    run(
        &mut conn,
        &mut fixture.engine,
        &mut client,
        "CREATE SEQUENCE numbers",
    );
    let response = run(
        &mut holder,
        &mut fixture.engine,
        &mut holder_client,
        "BEGIN; SELECT pg_advisory_xact_lock(7)",
    );
    assert!(messages(&response).iter().all(|message| message.0 != b'E'));
    client
        .write_all(&query(
            "SELECT id, CASE WHEN id > 1000 THEN nextval('numbers') END, \
             CASE WHEN id = 2000 THEN pg_advisory_xact_lock(7)::text END, payload FROM big",
        ))
        .unwrap();
    assert_eq!(readable(&mut conn, &mut fixture.engine), After::Continue);
    assert!(conn.result_streams.is_active());
    // Read until the stream waits for the lock with rows already delivered.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut received = Vec::new();
    while conn.parked_wait() != Some(crate::sql::StatementWait::Lock) {
        assert!(Instant::now() < deadline, "stream did not reach the lock");
        read_available(&mut client, &mut received);
        assert_eq!(conn.on_writable(), After::Continue);
        let generation = fixture.engine.lock_generation();
        if let Some(pending) = conn.retry_parked(&mut fixture.engine, KEY, generation, false) {
            conn.finish_response(pending, None);
        }
    }
    assert!(conn.result_streams.is_active());
    run(
        &mut holder,
        &mut fixture.engine,
        &mut holder_client,
        "COMMIT",
    );
    let received = drain_after(&mut conn, &mut fixture.engine, &mut client, 1, received);
    let response = messages(&received);
    let rows: Vec<_> = response
        .iter()
        .filter(|message| message.0 == b'D')
        .collect();
    assert_rows(&rows, 5000);
    for (index, row) in rows.iter().enumerate().skip(1000) {
        let payload = row.1;
        let first = 6 + i32::from_be_bytes(payload[2..6].try_into().unwrap()) as usize;
        let length = i32::from_be_bytes(payload[first..first + 4].try_into().unwrap()) as usize;
        let value = core::str::from_utf8(&payload[first + 4..first + 4 + length]).unwrap();
        assert_eq!(value, (index - 999).to_string(), "row {}", index + 1);
    }
    let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
    assert_eq!(&kinds[kinds.len() - 2..], b"CZ");
}

#[test]
fn publication_hold_keeps_output_after_early_writes_queued() {
    let config = Config::default_dev();
    let mut budget = Budget::new(256 << 20);
    let mut conn = Conn::new(&config, &mut budget).unwrap();
    assert!(conn.send.append(b"Z\0\0\0\x05I"));
    // Early writes passed the hold mark: what remains follows the mark and
    // must wait for publication instead of becoming flushable.
    conn.publication_hold = Some(OutputMark::new(0, 0));
    conn.overflow.record_written(10);
    assert_eq!(conn.flushable_bytes(), 0);
    assert!(!conn.wants_write());
    conn.publication_hold = Some(OutputMark::new(6, conn.overflow.written()));
    assert_eq!(conn.flushable_bytes(), 6);
    conn.publication_hold = None;
    assert!(conn.wants_write());
}

#[test]
fn paged_portal_rows_use_the_spool_and_fail_by_name_when_it_is_full() {
    for (spool_bytes, exhausted) in [(16 << 20, false), (1024, true)] {
        let mut config = fixture_config(
            crate::config::TransactionMode::Mvcc,
            &format!("paged-{exhausted}"),
            spool_bytes,
        );
        config.conn_send_buffer_bytes = 16 << 10;
        let mut fixture = Fixture::with_config(config);
        let (mut conn, mut client) = connection(&mut fixture, 1);
        let mut input = frontend(
            wire::FMSG_PARSE,
            b"\0SELECT repeat('x', 1000), count(*) OVER () FROM generate_series(1, 60)\0\0\0",
        );
        input.extend_from_slice(&frontend(wire::FMSG_BIND, b"\0\0\0\0\0\0\0\0"));
        input.extend_from_slice(&frontend(wire::FMSG_EXECUTE, b"\0\0\0\0\x32"));
        input.extend_from_slice(&frontend(wire::FMSG_SYNC, b""));
        client.write_all(&input).unwrap();
        assert_eq!(readable(&mut conn, &mut fixture.engine), After::Continue);
        let response = drain(&mut conn, &mut fixture.engine, &mut client, 1);
        let response = messages(&response);
        let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
        let rows = kinds.iter().filter(|kind| **kind == b'D').count();
        if exhausted {
            assert!(rows < 50);
            assert_eq!(&kinds[kinds.len() - 2..], b"EZ");
            let error = response[kinds.len() - 2].1;
            assert_eq!(sqlstate_of(error), "54000");
            assert!(
                error
                    .windows(b"result_spool_bytes".len())
                    .any(|part| part == b"result_spool_bytes")
            );
        } else {
            assert_eq!(rows, 50);
            assert_eq!(&kinds[kinds.len() - 2..], b"sZ");
        }
        let response = run(&mut conn, &mut fixture.engine, &mut client, "SELECT 5");
        assert!(
            messages(&response)
                .iter()
                .any(|message| message.0 == b'D' && first_text(message.1) == "5")
        );
    }
}

#[test]
fn queued_output_does_not_turn_a_lock_wait_into_a_client_wait() {
    let mut fixture = fixture(crate::config::TransactionMode::Mvcc, "lock-park", 16 << 20);
    let (mut conn, mut client) = connection(&mut fixture, 1);
    let (mut holder, mut holder_client) = connection(&mut fixture, 2);
    run(
        &mut holder,
        &mut fixture.engine,
        &mut holder_client,
        "BEGIN; SELECT pg_advisory_xact_lock(7)",
    );
    let mut sql = String::from(BIG);
    sql.push_str("; SELECT pg_advisory_xact_lock(7)");
    client.write_all(&query(&sql)).unwrap();
    assert_eq!(readable(&mut conn, &mut fixture.engine), After::Continue);
    assert_eq!(conn.parked_wait(), Some(crate::sql::StatementWait::Lock));
    assert!(conn.overflow_bytes() > 0);
    // A wake whose lock is still held keeps the statement waiting for it.
    let generation = fixture.engine.lock_generation() + 1;
    if let Some(pending) = conn.retry_parked(&mut fixture.engine, KEY, generation, false) {
        conn.finish_response(pending, None);
    }
    assert_eq!(conn.parked_wait(), Some(crate::sql::StatementWait::Lock));
    assert!(conn.cancel_parked(&mut fixture.engine));
    let response = drain(&mut conn, &mut fixture.engine, &mut client, 1);
    let response = messages(&response);
    let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
    assert_eq!(&kinds[kinds.len() - 2..], b"EZ");
    assert_eq!(sqlstate_of(response[kinds.len() - 2].1), "57014");
    run(
        &mut holder,
        &mut fixture.engine,
        &mut holder_client,
        "COMMIT",
    );
}

#[test]
fn routine_replays_never_repeat_or_panic_after_rows_reach_the_client() {
    let mut fixture = fixture(
        crate::config::TransactionMode::Mvcc,
        "routine-replay",
        16 << 20,
    );
    let (mut conn, mut client) = connection(&mut fixture, 1);
    run(
        &mut conn,
        &mut fixture.engine,
        &mut client,
        "CREATE TABLE audit(v int); CREATE FUNCTION audit_row(v int) RETURNS int \
         LANGUAGE sql VOLATILE AS 'INSERT INTO audit VALUES ($1) RETURNING $1'",
    );
    let count = |conn: &mut Conn, engine: &mut Engine, client: &mut TcpStream| {
        let response = run(conn, engine, client, "SELECT count(*) FROM audit");
        messages(&response)
            .iter()
            .find(|message| message.0 == b'D')
            .map(|message| first_text(message.1).to_string())
            .unwrap()
    };
    // Function-source rows hand each routine call off on its own row, so
    // calls from row 1 onward never restart the statement: no row repeats
    // and the work stays linear in the row count.
    let response = run(
        &mut conn,
        &mut fixture.engine,
        &mut client,
        "SELECT g, repeat('x', 200), CASE WHEN g <= 300 THEN audit_row(g) END \
         FROM generate_series(1, 5000) g",
    );
    let response = messages(&response);
    let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
    assert_eq!(&kinds[kinds.len() - 2..], b"CZ");
    let rows: Vec<_> = response
        .iter()
        .filter(|message| message.0 == b'D')
        .collect();
    assert_rows(&rows, 5000);
    assert_eq!(count(&mut conn, &mut fixture.engine, &mut client), "300");
    // A call after rows already reached the client is equally safe there.
    let response = run(
        &mut conn,
        &mut fixture.engine,
        &mut client,
        "SELECT g, repeat('x', 200), CASE WHEN g = 4000 THEN audit_row(g) END \
         FROM generate_series(1, 5000) g",
    );
    let response = messages(&response);
    let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
    assert_eq!(&kinds[kinds.len() - 2..], b"CZ");
    assert_eq!(count(&mut conn, &mut fixture.engine, &mut client), "301");
    // A retained join hands its calls off per row too, before and after
    // rows reach the client.
    for (call_at, total) in [(1, "302"), (4000, "303")] {
        let response = run(
            &mut conn,
            &mut fixture.engine,
            &mut client,
            &format!(
                "SELECT g, repeat('x', 200), CASE WHEN g = {call_at} THEN audit_row(g) END \
                 FROM generate_series(1, 5000) g CROSS JOIN (VALUES (1)) v"
            ),
        );
        let response = messages(&response);
        let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
        assert_eq!(&kinds[kinds.len() - 2..], b"CZ");
        let rows: Vec<_> = response
            .iter()
            .filter(|message| message.0 == b'D')
            .collect();
        assert_rows(&rows, 5000);
        assert_eq!(count(&mut conn, &mut fixture.engine, &mut client), total);
    }
    // A RIGHT JOIN still restarts the statement for a routine's first call.
    // Before any row leaves, the restart completes without repeating rows.
    let response = run(
        &mut conn,
        &mut fixture.engine,
        &mut client,
        "SELECT g, repeat('x', 200), CASE WHEN g = 1 THEN audit_row(g) END \
         FROM (VALUES (1)) v RIGHT JOIN generate_series(1, 5000) g ON true",
    );
    let response = messages(&response);
    let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
    assert_eq!(&kinds[kinds.len() - 2..], b"CZ");
    let rows: Vec<_> = response
        .iter()
        .filter(|message| message.0 == b'D')
        .collect();
    assert_rows(&rows, 5000);
    assert_eq!(count(&mut conn, &mut fixture.engine, &mut client), "304");
    // After rows already left it cannot restart: it fails loudly instead of
    // repeating rows, and its call rolls back.
    let response = run(
        &mut conn,
        &mut fixture.engine,
        &mut client,
        "SELECT g, repeat('x', 200), CASE WHEN g = 4000 THEN audit_row(g) END \
         FROM (VALUES (1)) v RIGHT JOIN generate_series(1, 5000) g ON true",
    );
    let response = messages(&response);
    let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
    assert_eq!(&kinds[kinds.len() - 2..], b"EZ");
    assert_eq!(sqlstate_of(response[kinds.len() - 2].1), "0A000");
    let rows: Vec<_> = response
        .iter()
        .filter(|message| message.0 == b'D')
        .collect();
    assert_rows(&rows, rows.len());
    assert_eq!(count(&mut conn, &mut fixture.engine, &mut client), "304");
}

#[test]
fn long_streams_release_each_rows_effect_receipts() {
    let mut config = fixture_config(
        crate::config::TransactionMode::Mvcc,
        "stream-receipts",
        16 << 20,
    );
    config.result_stream_bytes = 4 << 20;
    // Far less statement memory than one receipt per delivered row needs.
    config.sql_arena_bytes = 512 << 10;
    config.row_overlay_slots = 65536;
    let mut fixture = Fixture::with_config(config);
    let (mut conn, mut client) = connection(&mut fixture, 1);
    let response = run(
        &mut conn,
        &mut fixture.engine,
        &mut client,
        "CREATE TABLE long_rows(id int, payload text); CREATE SEQUENCE numbers",
    );
    assert!(messages(&response).iter().all(|message| message.0 != b'E'));
    let response = run(
        &mut conn,
        &mut fixture.engine,
        &mut client,
        "INSERT INTO long_rows SELECT g, repeat('x', 200) FROM generate_series(1, 6000) g",
    );
    assert!(
        messages(&response).iter().all(|message| message.0 != b'E'),
        "{}",
        String::from_utf8_lossy(&response)
    );
    client
        .write_all(&query(
            "SELECT id, CASE WHEN id > 1000 THEN nextval('numbers') + nextval('numbers') END, \
             payload FROM long_rows",
        ))
        .unwrap();
    assert_eq!(readable(&mut conn, &mut fixture.engine), After::Continue);
    assert!(conn.result_streams.is_active());
    let response = drain(&mut conn, &mut fixture.engine, &mut client, 1);
    let response = messages(&response);
    let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
    assert_eq!(
        &kinds[kinds.len() - 2..],
        b"CZ",
        "{}",
        String::from_utf8_lossy(response[kinds.len() - 2].1)
    );
    let rows: Vec<_> = response
        .iter()
        .filter(|message| message.0 == b'D')
        .collect();
    assert_rows(&rows, 6000);
}

#[test]
fn tls_result_streams_resume_while_the_client_reads_ciphertext() {
    let (mut fixture, _, _, mut other, mut other_client) =
        stream_fixture(crate::config::TransactionMode::Mvcc, "stream-tls");
    let (mut socket, server) = socket_pair();
    let mut conn = Conn::new(&fixture.config, &mut fixture.budget).unwrap();
    conn.open(server, 3).unwrap();
    conn.auth_database = fixture.engine.database_login("postgres");
    conn.phase = Phase::Ready;
    let mut client = tls_client(&mut conn, &mut socket);
    let mut input = query("SELECT id, payload FROM big");
    input.extend_from_slice(&query("SELECT 2"));
    client.writer().write_all(&input).unwrap();
    while client.wants_write() {
        client.write_tls(&mut socket).unwrap();
    }
    assert_eq!(readable(&mut conn, &mut fixture.engine), After::Continue);
    assert!(conn.result_streams.is_active());
    assert_eq!(
        conn.parked_wait(),
        Some(crate::sql::StatementWait::ClientOutput)
    );
    // The stream keeps at most one row beyond the send buffer and the
    // session's ciphertext; other sessions keep running meanwhile.
    assert!(conn.overflow_bytes() < 1024);
    let response = run(
        &mut other,
        &mut fixture.engine,
        &mut other_client,
        "SELECT count(*) FROM big",
    );
    assert!(messages(&response).iter().all(|message| message.0 != b'E'));
    assert!(conn.result_streams.is_active());
    let mut plaintext = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    while messages(&plaintext)
        .iter()
        .filter(|message| message.0 == b'Z')
        .count()
        < 2
    {
        assert!(Instant::now() < deadline, "TLS stream did not drain");
        read_tls_available(&mut client, &mut socket, &mut plaintext);
        assert_eq!(
            crate::mem::guard::forbid_alloc(|| conn.on_writable()),
            After::Continue
        );
        let generation = fixture.engine.lock_generation();
        if let Some(pending) = crate::mem::guard::forbid_alloc(|| {
            conn.retry_parked(&mut fixture.engine, KEY, generation, false)
        }) {
            assert_eq!(
                crate::mem::guard::forbid_alloc(|| conn.finish_response(pending, None)),
                After::Continue
            );
        }
    }
    let response = messages(&plaintext);
    let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
    let first_ready = kinds.iter().position(|kind| *kind == b'Z').unwrap();
    let rows: Vec<_> = response[..first_ready]
        .iter()
        .filter(|message| message.0 == b'D')
        .collect();
    assert_rows(&rows, 5000);
    assert_eq!(response[first_ready - 1].1, b"SELECT 5000\0");
    assert_eq!(&kinds[first_ready + 1..], b"TDCZ");
    assert!(!conn.result_streams.is_active());
}

#[test]
fn a_stream_whose_open_outgrows_its_estimate_spools_instead() {
    let mut config = fixture_config(
        crate::config::TransactionMode::Mvcc,
        "stream-open-full",
        16 << 20,
    );
    config.result_stream_bytes = Config::MIN_RESULT_STREAM_BYTES * 3;
    let mut fixture = Fixture::with_config(config);
    let (mut conn, mut client) = connection(&mut fixture, 1);
    let response = run(
        &mut conn,
        &mut fixture.engine,
        &mut client,
        "CREATE TABLE narrow(id int, payload text); \
         INSERT INTO narrow SELECT g, repeat('x', 2000) FROM generate_series(1, 400) g",
    );
    assert!(messages(&response).iter().all(|message| message.0 != b'E'));
    // Each short column reference parses into far more than the per-byte
    // allowance the estimate makes for query text, so the estimate admits
    // the stream and binding its plan then outgrows the allocation.
    let mut sql = String::from("SELECT payload");
    for _ in 0..1500 {
        sql.push_str(",id");
    }
    sql.push_str(" FROM narrow");
    let before = fixture.engine.result_stream_metrics();
    let response = run(&mut conn, &mut fixture.engine, &mut client, &sql);
    let response = messages(&response);
    let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
    // Promotion discarded the first attempt and opening queued nothing, so
    // the statement runs again spooled and succeeds.
    assert_eq!(
        fixture.engine.result_stream_metrics().spooled_capacity,
        before.spooled_capacity + 1
    );
    assert_eq!(fixture.engine.result_stream_metrics().opened, before.opened);
    assert!(!kinds.contains(&b'E'), "{kinds:?}");
    assert_eq!(kinds[0], b'T');
    assert_eq!(kinds.iter().filter(|kind| **kind == b'D').count(), 400);
    assert_eq!(response[kinds.len() - 2].1, b"SELECT 400\0");
    assert!(!conn.result_streams.is_active());
    let response = run(&mut conn, &mut fixture.engine, &mut client, "SELECT 5");
    assert!(
        messages(&response)
            .iter()
            .any(|message| message.0 == b'D' && first_text(message.1) == "5")
    );
}

fn parse(statement: &str, sql: &str) -> Vec<u8> {
    let mut payload = statement.as_bytes().to_vec();
    payload.push(0);
    payload.extend_from_slice(sql.as_bytes());
    payload.extend_from_slice(b"\0\0\0");
    frontend(wire::FMSG_PARSE, &payload)
}

fn bind(portal: &str, statement: &str) -> Vec<u8> {
    let mut payload = portal.as_bytes().to_vec();
    payload.push(0);
    payload.extend_from_slice(statement.as_bytes());
    payload.extend_from_slice(b"\0\0\0\0\0\0\0");
    frontend(wire::FMSG_BIND, &payload)
}

fn execute(portal: &str, max_rows: i32) -> Vec<u8> {
    let mut payload = portal.as_bytes().to_vec();
    payload.push(0);
    payload.extend_from_slice(&max_rows.to_be_bytes());
    frontend(wire::FMSG_EXECUTE, &payload)
}

fn sync() -> Vec<u8> {
    frontend(wire::FMSG_SYNC, b"")
}

/// The second column of a DataRow as text.
fn second_text(payload: &[u8]) -> Option<&str> {
    let first = 6 + i32::from_be_bytes(payload[2..6].try_into().unwrap()) as usize;
    let length = i32::from_be_bytes(payload[first..first + 4].try_into().unwrap());
    (length >= 0)
        .then(|| core::str::from_utf8(&payload[first + 4..first + 4 + length as usize]).unwrap())
}

#[test]
fn paged_portals_evaluate_rows_per_execute_across_client_waits() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let (mut fixture, mut conn, mut client, _, _) = stream_fixture(mode, "paged-stream");
        run(
            &mut conn,
            &mut fixture.engine,
            &mut client,
            "CREATE SEQUENCE numbers",
        );
        let opened = fixture.engine.result_stream_metrics().opened;
        // The first row starts no effect, so the portal opens as a stream
        // and later rows take numbers only when an Execute asks for them.
        let mut input = parse(
            "",
            "SELECT id, CASE WHEN id > 1 THEN nextval('numbers') END, payload FROM big",
        );
        input.extend_from_slice(&bind("", ""));
        input.extend_from_slice(&execute("", 10));
        input.extend_from_slice(&parse("", "SELECT nextval('numbers')"));
        input.extend_from_slice(&bind("other", ""));
        input.extend_from_slice(&execute("other", 0));
        client.write_all(&input).unwrap();
        assert_eq!(readable(&mut conn, &mut fixture.engine), After::Continue);
        let mut received = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        while messages(&received)
            .iter()
            .filter(|message| message.0 == b'C')
            .count()
            == 0
        {
            assert!(Instant::now() < deadline, "first page did not arrive");
            read_available(&mut client, &mut received);
            assert_eq!(conn.on_writable(), After::Continue);
        }
        assert_eq!(fixture.engine.result_stream_metrics().opened, opened + 1);
        assert!(conn.result_streams.is_held());
        assert!(!conn.result_streams.is_active());
        {
            let response = messages(&received);
            let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
            assert_eq!(&kinds[..2], b"12");
            assert_eq!(&kinds[12..], b"s12DC");
            let rows: Vec<_> = response[..12]
                .iter()
                .filter(|message| message.0 == b'D')
                .collect();
            assert_rows(&rows, 10);
            // Rows 2..=10 took nine numbers; the other portal takes the tenth.
            assert_eq!(second_text(rows[9].1), Some("9"));
            assert_eq!(first_text(response[15].1), "10");
        }

        // A page larger than the send buffer waits for the client, then the
        // portal suspends again; the final unlimited Execute completes it.
        let mut input = execute("", 3000);
        input.extend_from_slice(&execute("", 0));
        input.extend_from_slice(&sync());
        client.write_all(&input).unwrap();
        assert_eq!(readable(&mut conn, &mut fixture.engine), After::Continue);
        assert_eq!(
            conn.parked_wait(),
            Some(crate::sql::StatementWait::ClientOutput)
        );
        assert!(conn.result_streams.is_active());
        let response = drain(&mut conn, &mut fixture.engine, &mut client, 1);
        let response = messages(&response);
        let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
        let suspended = kinds.iter().position(|kind| *kind == b's').unwrap();
        assert_eq!(suspended, 3000);
        assert_eq!(&kinds[kinds.len() - 2..], b"CZ");
        assert_eq!(response[kinds.len() - 2].1, b"SELECT 1990\0");
        let rows: Vec<_> = response
            .iter()
            .filter(|message| message.0 == b'D')
            .collect();
        assert_eq!(rows.len(), 4990);
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(first_text(row.1), (index + 11).to_string());
            assert_eq!(second_text(row.1), Some((index + 11).to_string().as_str()));
        }
        // Sync ended the implicit transaction and with it the portal.
        assert!(!conn.result_streams.is_held());
    }
}

#[test]
fn suspended_portals_hold_their_table_and_share_the_stream_slots() {
    let (mut fixture, mut conn, mut client, _, _) =
        stream_fixture(crate::config::TransactionMode::Mvcc, "paged-hold");
    assert_eq!(fixture.config.result_stream_slots, 2);
    let before = fixture.engine.result_stream_metrics();
    let mut input = query("BEGIN");
    input.extend_from_slice(&parse("first", "SELECT id, payload FROM big"));
    input.extend_from_slice(&bind("a", "first"));
    input.extend_from_slice(&execute("a", 2));
    // The second stream slot holds this portal too.
    input.extend_from_slice(&parse(
        "second",
        "SELECT id, payload FROM big WHERE id <= 40",
    ));
    input.extend_from_slice(&bind("b", "second"));
    input.extend_from_slice(&execute("b", 3));
    // Both slots are held, so this portal pages from its own buffer.
    input.extend_from_slice(&parse(
        "third",
        "SELECT id, payload FROM big WHERE id <= 20",
    ));
    input.extend_from_slice(&bind("c", "third"));
    input.extend_from_slice(&execute("c", 2));
    input.extend_from_slice(&execute("a", 2));
    input.extend_from_slice(&execute("b", 3));
    input.extend_from_slice(&execute("c", 2));
    input.extend_from_slice(&sync());
    client.write_all(&input).unwrap();
    assert_eq!(readable(&mut conn, &mut fixture.engine), After::Continue);
    let response = drain(&mut conn, &mut fixture.engine, &mut client, 2);
    let response = messages(&response);
    let rows: Vec<&str> = response
        .iter()
        .filter(|message| message.0 == b'D')
        .map(|message| first_text(message.1))
        .collect();
    assert_eq!(
        rows,
        [
            "1", "2", "1", "2", "3", "1", "2", "3", "4", "4", "5", "6", "3", "4"
        ]
    );
    assert_eq!(
        response.iter().filter(|message| message.0 == b's').count(),
        6
    );
    let after = fixture.engine.result_stream_metrics();
    assert_eq!(after.opened, before.opened + 2);
    assert_eq!(after.spooled_capacity, before.spooled_capacity + 1);
    assert!(conn.result_streams.is_held());
    assert!(conn.result_streams.free().is_none());

    // DDL on the table of a suspended portal fails as in PostgreSQL; the
    // failed transaction keeps the portal from running until it ends.
    let response = run(
        &mut conn,
        &mut fixture.engine,
        &mut client,
        "DROP TABLE big",
    );
    let response = messages(&response);
    assert_eq!(response[0].0, b'E');
    assert_eq!(sqlstate_of(response[0].1), "55006");
    let mut input = execute("a", 2);
    input.extend_from_slice(&sync());
    client.write_all(&input).unwrap();
    assert_eq!(readable(&mut conn, &mut fixture.engine), After::Continue);
    let response = drain(&mut conn, &mut fixture.engine, &mut client, 1);
    let response = messages(&response);
    assert_eq!(response[0].0, b'E');
    assert_eq!(sqlstate_of(response[0].1), "25P02");
    let response = run(&mut conn, &mut fixture.engine, &mut client, "ROLLBACK");
    assert!(messages(&response).iter().all(|message| message.0 != b'E'));
    assert!(!conn.result_streams.is_held());
    let response = run(
        &mut conn,
        &mut fixture.engine,
        &mut client,
        "DROP TABLE big",
    );
    assert!(messages(&response).iter().all(|message| message.0 != b'E'));
}

#[test]
fn buffered_paged_portals_name_portal_result_bytes_when_full() {
    let (mut fixture, mut conn, mut client, _, _) =
        stream_fixture(crate::config::TransactionMode::Mvcc, "paged-full");
    // A subquery is not a retained shape: the paged portal buffers its result.
    let mut input = parse(
        "",
        "SELECT id, payload FROM big WHERE id > (SELECT 0) ORDER BY id",
    );
    input.extend_from_slice(&bind("", ""));
    input.extend_from_slice(&execute("", 10));
    input.extend_from_slice(&sync());
    client.write_all(&input).unwrap();
    assert_eq!(readable(&mut conn, &mut fixture.engine), After::Continue);
    let response = drain(&mut conn, &mut fixture.engine, &mut client, 1);
    let response = messages(&response);
    let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
    assert_eq!(kinds, b"12EZ");
    assert_eq!(sqlstate_of(response[2].1), "54000");
    let message = String::from_utf8_lossy(response[2].1);
    assert!(message.contains("portal_result_bytes"), "{message}");
}

#[test]
fn forward_fetches_stream_cursor_rows_as_the_client_reads() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let (mut fixture, mut conn, mut client, mut other, mut other_client) =
            stream_fixture(mode, "fetch-stream");
        run(
            &mut conn,
            &mut fixture.engine,
            &mut client,
            "CREATE SEQUENCE numbers",
        );
        let opened = fixture.engine.result_stream_metrics().opened;
        // FETCH ALL's megabyte exceeds cursor_bytes; streamed, no row of it
        // is saved, and each row takes its number as it is evaluated.
        let mut input = query(
            "BEGIN; DECLARE c NO SCROLL CURSOR FOR \
             SELECT id, nextval('numbers'), payload FROM big; FETCH 10 c; FETCH ALL c",
        );
        input.extend_from_slice(&query("FETCH 5 c; COMMIT"));
        client.write_all(&input).unwrap();
        assert_eq!(readable(&mut conn, &mut fixture.engine), After::Continue);
        assert_eq!(
            conn.parked_wait(),
            Some(crate::sql::StatementWait::ClientOutput)
        );
        assert!(conn.result_streams.is_active());
        assert!(conn.overflow_bytes() < 1024);
        let progress = if mode == crate::config::TransactionMode::Mvcc {
            "SELECT count(*) FROM big"
        } else {
            "SELECT 1"
        };
        let response = run(&mut other, &mut fixture.engine, &mut other_client, progress);
        assert!(messages(&response).iter().all(|message| message.0 != b'E'));
        let response = drain(&mut conn, &mut fixture.engine, &mut client, 2);
        let response = messages(&response);
        let tags: Vec<&[u8]> = response
            .iter()
            .filter(|message| message.0 == b'C')
            .map(|message| message.1)
            .collect();
        assert_eq!(
            tags,
            [
                &b"BEGIN\0"[..],
                b"DECLARE CURSOR\0",
                b"FETCH 10\0",
                b"FETCH 4990\0",
                b"FETCH 0\0",
                b"COMMIT\0"
            ]
        );
        let rows: Vec<_> = response
            .iter()
            .filter(|message| message.0 == b'D')
            .collect();
        assert_rows(&rows, 5000);
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(second_text(row.1), Some((index + 1).to_string().as_str()));
        }
        assert_eq!(
            response.iter().filter(|message| message.0 == b'T').count(),
            3
        );
        assert!(fixture.engine.result_stream_metrics().opened >= opened + 2);
        assert!(!conn.result_streams.is_held());
    }
}

#[test]
fn scrollable_fetches_stream_forward_and_reread_input_to_move_back() {
    let (mut fixture, mut conn, mut client, _, _) =
        stream_fixture(crate::config::TransactionMode::Mvcc, "scroll-fetch-stream");
    let opened = fixture.engine.result_stream_metrics().opened;
    // A table scan's cursor is scrollable by default. FETCH ALL streams
    // without saving its rows; moving back reads the input again with the
    // cursor's snapshot, so a later insert is not seen.
    let mut input =
        query("BEGIN; DECLARE c CURSOR FOR SELECT id, payload FROM big; FETCH 10 c; FETCH ALL c");
    input.extend_from_slice(&query(
        "INSERT INTO big VALUES (9001, 'late'); FETCH PRIOR c; FETCH ABSOLUTE 3 c; \
         FETCH BACKWARD 2 c; FETCH LAST c; COMMIT",
    ));
    client.write_all(&input).unwrap();
    assert_eq!(readable(&mut conn, &mut fixture.engine), After::Continue);
    assert!(conn.result_streams.is_active());
    assert!(conn.overflow_bytes() < 1024);
    let response = drain(&mut conn, &mut fixture.engine, &mut client, 2);
    let response = messages(&response);
    let tags: Vec<&[u8]> = response
        .iter()
        .filter(|message| message.0 == b'C')
        .map(|message| message.1)
        .collect();
    assert_eq!(
        tags,
        [
            &b"BEGIN\0"[..],
            b"DECLARE CURSOR\0",
            b"FETCH 10\0",
            b"FETCH 4990\0",
            b"INSERT 0 1\0",
            b"FETCH 1\0",
            b"FETCH 1\0",
            b"FETCH 2\0",
            b"FETCH 1\0",
            b"COMMIT\0"
        ]
    );
    let rows: Vec<&str> = response
        .iter()
        .filter(|message| message.0 == b'D')
        .map(|message| first_text(message.1))
        .collect();
    assert_eq!(rows.len(), 5005);
    assert_eq!(&rows[5000..], ["5000", "3", "2", "1", "5000"]);
    assert!(fixture.engine.result_stream_metrics().opened > opened);
}

#[test]
fn streamed_fetches_fail_their_cursor_after_delivered_rows() {
    let (mut fixture, mut conn, mut client, _, _) =
        stream_fixture(crate::config::TransactionMode::Mvcc, "fetch-stream-error");
    let response = run(
        &mut conn,
        &mut fixture.engine,
        &mut client,
        "BEGIN; DECLARE c NO SCROLL CURSOR FOR SELECT id, 1 / (id - 3001), payload FROM big; \
         FETCH ALL c",
    );
    let response = messages(&response);
    let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
    assert_eq!(&kinds[kinds.len() - 2..], b"EZ");
    assert_eq!(sqlstate_of(response[kinds.len() - 2].1), "22012");
    let rows: Vec<_> = response
        .iter()
        .filter(|message| message.0 == b'D')
        .collect();
    assert_rows(&rows, 3000);
    assert_eq!(response[kinds.len() - 1].1, b"E");
    let response = run(&mut conn, &mut fixture.engine, &mut client, "ROLLBACK");
    assert!(messages(&response).iter().all(|message| message.0 != b'E'));
}

/// The data lines of CopyData messages.
fn copy_lines<'a>(messages: &[(u8, &'a [u8])]) -> Vec<&'a str> {
    messages
        .iter()
        .filter(|message| message.0 == b'd')
        .map(|message| {
            core::str::from_utf8(message.1)
                .unwrap()
                .trim_end_matches('\n')
        })
        .collect()
}

#[test]
fn copy_to_stdout_streams_rows_as_the_client_reads() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let (mut fixture, mut conn, mut client, mut other, mut other_client) =
            stream_fixture(mode, "copy-stream");
        let opened = fixture.engine.result_stream_metrics().opened;
        let mut input = query("COPY big TO STDOUT");
        input.extend_from_slice(&query(
            "COPY (SELECT id, payload FROM big WHERE id > 4000) TO STDOUT (FORMAT csv, HEADER)",
        ));
        client.write_all(&input).unwrap();
        assert_eq!(readable(&mut conn, &mut fixture.engine), After::Continue);
        assert_eq!(
            conn.parked_wait(),
            Some(crate::sql::StatementWait::ClientOutput)
        );
        assert!(conn.result_streams.is_active());
        assert!(conn.overflow_bytes() < 1024);
        let progress = if mode == crate::config::TransactionMode::Mvcc {
            "SELECT count(*) FROM big"
        } else {
            "SELECT 1"
        };
        let response = run(&mut other, &mut fixture.engine, &mut other_client, progress);
        assert!(messages(&response).iter().all(|message| message.0 != b'E'));
        let response = drain(&mut conn, &mut fixture.engine, &mut client, 2);
        let response = messages(&response);
        let first_ready = response
            .iter()
            .position(|message| message.0 == b'Z')
            .unwrap();
        let (table, select) = response.split_at(first_ready + 1);
        let kinds: Vec<u8> = table.iter().map(|message| message.0).collect();
        assert_eq!(kinds[0], b'H');
        assert_eq!(&kinds[kinds.len() - 3..], b"cCZ");
        assert_eq!(table[kinds.len() - 2].1, b"COPY 5000\0");
        let lines = copy_lines(table);
        assert_eq!(lines.len(), 5000);
        for (index, line) in lines.iter().enumerate() {
            assert_eq!(*line, format!("{}\t{}", index + 1, "x".repeat(200)));
        }
        let lines = copy_lines(select);
        assert_eq!(lines.len(), 1001);
        assert_eq!(lines[0], "id,payload");
        assert_eq!(lines[1], format!("4001,{}", "x".repeat(200)));
        assert_eq!(select[select.len() - 2].1, b"COPY 1000\0");
        assert_eq!(fixture.engine.result_stream_metrics().opened, opened + 2);
        assert!(!conn.result_streams.is_held());
    }
}

#[test]
fn streamed_copy_fails_after_delivered_rows_and_matches_unstreamed_output() {
    let (mut fixture, mut conn, mut client, _, _) =
        stream_fixture(crate::config::TransactionMode::Mvcc, "copy-stream-error");
    let response = run(
        &mut conn,
        &mut fixture.engine,
        &mut client,
        "COPY (SELECT id, 1 / (id - 3001), payload FROM big) TO STDOUT",
    );
    let response = messages(&response);
    let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
    assert_eq!(&kinds[kinds.len() - 2..], b"EZ");
    assert_eq!(sqlstate_of(response[kinds.len() - 2].1), "22012");
    assert_eq!(copy_lines(&response).len(), 3000);
    // Binary output streamed from the table equals the same rows spooled.
    let spooled = fixture.engine.result_stream_metrics().spooled_shape;
    let streamed = run(
        &mut conn,
        &mut fixture.engine,
        &mut client,
        "COPY big (payload, id) TO STDOUT (FORMAT binary)",
    );
    let small = run(
        &mut conn,
        &mut fixture.engine,
        &mut client,
        "COPY (SELECT payload, id FROM big WHERE id > (SELECT 0) ORDER BY id) \
         TO STDOUT (FORMAT binary)",
    );
    assert_eq!(
        fixture.engine.result_stream_metrics().spooled_shape,
        spooled + 1
    );
    let data = |response: &[u8]| -> Vec<u8> {
        messages(response)
            .iter()
            .filter(|message| message.0 == b'd')
            .flat_map(|message| message.1.iter().copied())
            .collect()
    };
    assert_eq!(data(&streamed), data(&small));
    assert!(data(&streamed).starts_with(b"PGCOPY\n\xff\r\n\0"));
}
