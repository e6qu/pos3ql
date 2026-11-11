//! Result streams yield on cold object reads after delivering rows, while
//! other sessions keep running.

use super::object_reads::{advance, read_request, replace_uncached_blocks, reply};
use super::*;
use crate::pg::conn::{After, AuthContext, Conn};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

const KEY: &[u8; 16] = b"result-stream-ky";

fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325u64, |state, byte| {
        (state ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

fn query(sql: &str) -> Vec<u8> {
    let mut message = vec![b'Q'];
    message.extend_from_slice(&((sql.len() + 5) as i32).to_be_bytes());
    message.extend_from_slice(sql.as_bytes());
    message.push(0);
    message
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

fn fields(payload: &[u8]) -> Vec<&[u8]> {
    let count = u16::from_be_bytes(payload[..2].try_into().unwrap()) as usize;
    let mut at = 2;
    let mut values = Vec::new();
    for _ in 0..count {
        let length = i32::from_be_bytes(payload[at..at + 4].try_into().unwrap()) as usize;
        values.push(&payload[at + 4..at + 4 + length]);
        at += 4 + length;
    }
    values
}

fn socket_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    server.set_nonblocking(true).unwrap();
    client.set_nonblocking(true).unwrap();
    (client, server)
}

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

fn message(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut message = vec![kind];
    message.extend_from_slice(&((payload.len() + 4) as i32).to_be_bytes());
    message.extend_from_slice(payload);
    message
}

/// Parse, Bind, three Executes of `page` rows, an unlimited Execute and Sync.
fn paged_request(sql: &str, page: i32) -> Vec<u8> {
    let mut parse = b"\0".to_vec();
    parse.extend_from_slice(sql.as_bytes());
    parse.extend_from_slice(b"\0\0\0");
    let mut request = message(b'P', &parse);
    request.extend_from_slice(&message(b'B', b"\0\0\0\0\0\0\0\0"));
    let mut execute = b"\0".to_vec();
    execute.extend_from_slice(&page.to_be_bytes());
    for _ in 0..3 {
        request.extend_from_slice(&message(b'E', &execute));
    }
    request.extend_from_slice(&message(b'E', b"\0\0\0\0\0"));
    request.extend_from_slice(&message(b'S', b""));
    request
}

#[test]
fn result_streams_yield_on_cold_reads_after_delivered_rows() {
    cold_result_stream("result-stream-cold", false);
}

#[test]
fn paged_portals_yield_on_cold_reads_between_and_within_pages() {
    cold_result_stream("result-stream-cold-paged", true);
}

fn cold_result_stream(name: &str, paged: bool) {
    let mut config = test_config(name);
    config.object_store_on = true;
    config.object_store_sim = true;
    config.object_store_bucket = format!("{name}-{}", std::process::id());
    config.wal_upload = true;
    config.wal_upload_sync = true;
    config.object_store_get_slots = 4;
    config.object_store_hedge_after_ms = 0;
    config.block_cache_bytes = 0;
    config.disk_cache_bytes = 0;
    config.max_spill_generations_per_table = 2;
    config.memtable_bytes = 8 << 20;
    config.result_stream_bytes = 256 << 10;
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    let namespace = crate::object_store::sim::open_namespace(&config.object_store_bucket, 229);
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE cold_orders(id integer, code text)",
        false,
    );
    let mut insert = String::from("INSERT INTO cold_orders VALUES ");
    let mut expected = Vec::new();
    for id in 1..=320 {
        if id != 1 {
            insert.push(',');
        }
        insert.push_str(&format!("({id},'"));
        let start = insert.len();
        let mut random = id as u64;
        for _ in 0..1024 {
            random = random
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            insert.push(char::from(b'a' + ((random >> 32) % 26) as u8));
        }
        expected.push(checksum(&insert.as_bytes()[start..]));
        insert.push_str("')");
    }
    session.success(&mut engine, &insert, false);
    assert!(engine.checkpoint().unwrap());
    drop((session, engine));
    std::fs::remove_dir_all(&config.data_dir).unwrap();
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut other = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut network = config.clone();
    network.object_store_sim = false;
    network.object_store_endpoint = listener.local_addr().unwrap().to_string();
    network.object_store_access_key = "test-access".to_owned();
    network.object_store_secret_key = "test-secret".to_owned();
    replace_uncached_blocks(&mut engine, &network);
    let prefix = format!("/{}/", config.object_store_bucket);

    let mut conn = Conn::new(&config, &mut budget).unwrap();
    let (mut client, server) = socket_pair();
    conn.open(server, 1);
    conn.enter_ready_for_tests(&engine);
    let auth = AuthContext {
        mode: crate::pg::auth::AuthMode::Trust,
        password: String::new(),
        scram: None,
    };
    let sql = "SELECT id, code FROM cold_orders";
    client
        .write_all(&if paged {
            paged_request(sql, 50)
        } else {
            query(sql)
        })
        .unwrap();
    let mut poll = libc::pollfd {
        fd: std::os::fd::AsRawFd::as_raw_fd(conn.stream()),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut poll, 1, 5000) }, 1);
    engine.bind_query_connection(conn.id());
    let pending =
        crate::mem::guard::forbid_alloc(|| conn.on_readable(&mut engine, KEY, &auth, None));
    assert_eq!(conn.finish_response(pending, None), After::Continue);

    let mut received = Vec::new();
    let mut yielded_after_rows = false;
    let mut deadline = Instant::now() + Duration::from_secs(10);
    loop {
        read_available(&mut client, &mut received);
        let delivered = messages(&received)
            .iter()
            .filter(|message| message.0 == b'D')
            .count();
        if messages(&received).iter().any(|message| message.0 == b'Z') {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cold result stream did not finish"
        );
        if conn.parked_wait() == Some(StatementWait::ObjectIo)
            && conn.result_stream_active()
            && (!paged || delivered > 50)
            && delivered > 0
            && !yielded_after_rows
        {
            yielded_after_rows = true;
            // The stream owns a pending GET, yet the worker serves another
            // session's cached query in the meantime.
            assert!(engine.block_reads_pending());
            assert_eq!(
                data_rows(&other.success(&mut engine, "SELECT 42", true)),
                ["42"]
            );
            engine.bind_query_connection(conn.id());
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let request = read_request(&mut engine, &mut stream);
                let key = request
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .strip_prefix(&prefix)
                    .unwrap()
                    .to_owned();
                let body = namespace.borrow().object_bytes(&key).unwrap().to_vec();
                reply(&mut stream, &request, &body);
                deadline = Instant::now() + Duration::from_secs(10);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("accept object GET: {error}"),
        }
        advance(&mut engine);
        assert_eq!(
            crate::mem::guard::forbid_alloc(|| conn.on_writable()),
            After::Continue
        );
        engine.bind_query_connection(conn.id());
        let generation = engine.lock_generation();
        if let Some(pending) = crate::mem::guard::forbid_alloc(|| {
            conn.retry_parked(&mut engine, KEY, generation, true)
        }) {
            assert_eq!(conn.finish_response(pending, None), After::Continue);
        }
        std::thread::yield_now();
    }
    let response = messages(&received);
    let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
    assert_eq!(
        &kinds[kinds.len() - 2..],
        b"CZ",
        "{:?}",
        String::from_utf8_lossy(response[kinds.len() - 2].1)
    );
    if paged {
        // Each full page suspends; the unlimited Execute returns the rest.
        assert_eq!(kinds.iter().filter(|kind| **kind == b's').count(), 3);
        assert_eq!(response[kinds.len() - 2].1, b"SELECT 170\0");
    } else {
        assert_eq!(kinds.iter().filter(|kind| **kind == b'T').count(), 1);
        assert_eq!(response[kinds.len() - 2].1, b"SELECT 320\0");
    }
    let rows: Vec<_> = response
        .iter()
        .filter(|message| message.0 == b'D')
        .collect();
    assert_eq!(rows.len(), 320);
    for (index, row) in rows.iter().enumerate() {
        let values = fields(row.1);
        assert_eq!(values[0], (index + 1).to_string().as_bytes());
        assert_eq!(checksum(values[1]), expected[index]);
    }
    assert!(
        yielded_after_rows,
        "a cold read after delivered rows must yield instead of blocking"
    );
    assert!(engine.result_stream_metrics().opened >= 1);
    drop(conn.close());
    drop((other, engine));
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
}
