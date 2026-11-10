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

fn set_buffer(socket: &TcpStream, option: libc::c_int, bytes: libc::c_int) {
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
fn socket_pair() -> (TcpStream, TcpStream) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    set_buffer(&client, libc::SO_RCVBUF, 4096);
    set_buffer(&server, libc::SO_SNDBUF, 4096);
    server.set_nonblocking(true).unwrap();
    client.set_nonblocking(true).unwrap();
    (client, server)
}

struct Fixture {
    engine: Engine,
    budget: Budget,
    config: Config,
}

fn fixture(mode: crate::config::TransactionMode, name: &str, spool_bytes: usize) -> Fixture {
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
    config.row_overlay_slots = 64;
    config.result_spool_bytes = spool_bytes;
    config.result_spool_leases = 2;
    let mut budget = crate::sql::test_engine_budget(&config, 1 << 30);
    let engine = Engine::new(&config, &mut budget).unwrap();
    Fixture {
        engine,
        budget,
        config,
    }
}

fn connection(fixture: &mut Fixture, id: i32) -> (Conn, TcpStream) {
    let (client, server) = socket_pair();
    let mut conn = Conn::new(&fixture.config, &mut fixture.budget).unwrap();
    conn.open(server, id);
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
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut received = Vec::new();
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

const BIG: &str = "SELECT g, repeat('x', 200) FROM generate_series(1, 5000) g";

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
                "SELECT g, 1 / (g - 3001) FROM generate_series(1, 5000) g",
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
    let deadline = Instant::now() + Duration::from_secs(5);
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

#[test]
fn tls_backpressure_keeps_ciphertext_queued_instead_of_closing() {
    use crate::mem::lease_pool::{LeasePool, Purpose};
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
    let (mut socket, server) = socket_pair();
    let config = Config::default_dev();
    let mut budget = Budget::new(256 << 20);
    let mut conn = Conn::new(&config, &mut budget).unwrap();
    conn.open(server, 5);
    let mut session = crate::pg::tls::ServerSession::new(&server_config).unwrap();
    let mut client = rustls::ClientConnection::new(
        client_config,
        rustls::pki_types::ServerName::try_from("localhost").unwrap(),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while client.is_handshaking() || session.is_handshaking() {
        assert!(Instant::now() < deadline, "handshake stalled");
        while client.wants_write() {
            match client.write_tls(&mut socket) {
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
        match client.read_tls(&mut socket) {
            Ok(_) => {
                client.process_new_packets().unwrap();
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("client read: {error}"),
        }
    }
    conn.tls = Some(session);
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
        match client.read_tls(&mut socket) {
            Ok(0) => panic!("server closed the TLS connection"),
            Ok(_) => {
                client.process_new_packets().unwrap();
                let mut chunk = [0u8; 16384];
                loop {
                    match client.reader().read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => plaintext.extend_from_slice(&chunk[..n]),
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(error) => panic!("client plaintext: {error}"),
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("client TLS read: {error}"),
        }
    }
    assert_eq!(plaintext.len(), rows * 43);
    assert!(messages(&plaintext).iter().all(|message| message.0 == b'D'));
    drop(conn);
}
