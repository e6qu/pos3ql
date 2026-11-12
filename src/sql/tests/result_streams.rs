//! Result streams yield on cold object reads after delivering rows, while
//! other sessions keep running.

use super::object_reads::{HttpObjectServer, advance, replace_uncached_blocks};
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum ColdRequest {
    Select,
    Paged,
    Copy,
    /// A paged portal of a grouping, which reads its whole input first.
    Grouped,
    /// A paged portal of a plain DISTINCT, grouped by its output list.
    Distinct,
    /// A paged portal of a UNION ALL whose second query reads the table
    /// again from cold storage after the first query's rows were delivered.
    Union,
    /// The same UNION ALL as a simple Query: its stream opens before the
    /// statement runs, so no attempt reads the table first.
    UnionQuery,
    /// A simple Query over a derived table, which is merged into its query
    /// and streams as the table scan it became.
    Derived,
    /// A CTE read once, inlined into its query, then merged the same way.
    Cte,
    /// A view, expanded under its owner's privileges and merged.
    View,
    /// A paged portal of a hash join of the cold table with itself.
    Join,
    /// A simple Query of the same join.
    JoinQuery,
    /// A paged portal of a RIGHT join of the cold table with itself.
    RightJoin,
    /// A simple Query of the same RIGHT join.
    RightJoinQuery,
    /// A simple Query of a grouping, which reads its whole input first.
    GroupedQuery,
    /// A join with a bounded derived table, which the general executor
    /// materializes in an external run written to object storage. With no
    /// block cache, each restart after a cold read loses the blocks the last
    /// attempt read: past the restart bound the attempt reads synchronously
    /// and finishes.
    SpooledJoin,
    /// A `NOT MATERIALIZED` CTE read twice: both reads merge into one join
    /// of the table with itself, the second under a fresh name.
    CteTwice,
    /// The same self-join through two derived tables.
    DerivedSelfJoin,
}

#[test]
fn result_streams_yield_on_cold_reads_after_delivered_rows() {
    cold_result_stream("result-stream-cold", ColdRequest::Select);
}

#[test]
fn paged_portals_yield_on_cold_reads_between_and_within_pages() {
    cold_result_stream("result-stream-cold-paged", ColdRequest::Paged);
}

#[test]
fn copy_to_stdout_yields_on_cold_reads_after_delivered_rows() {
    cold_result_stream("result-stream-cold-copy", ColdRequest::Copy);
}

#[test]
fn grouped_paged_portals_yield_on_cold_reads_and_read_each_object_once() {
    cold_result_stream("result-stream-cold-grouped", ColdRequest::Grouped);
}

#[test]
fn union_all_streams_yield_on_cold_reads_in_each_query() {
    cold_result_stream("result-stream-cold-union", ColdRequest::Union);
}

#[test]
fn union_all_queries_open_their_stream_before_running() {
    cold_result_stream("result-stream-cold-union-query", ColdRequest::UnionQuery);
}

#[test]
fn merged_derived_tables_stream_like_their_table() {
    cold_result_stream("result-stream-cold-derived", ColdRequest::Derived);
}

#[test]
fn inlined_ctes_stream_like_their_table() {
    cold_result_stream("result-stream-cold-cte", ColdRequest::Cte);
}

#[test]
fn views_stream_like_their_table() {
    cold_result_stream("result-stream-cold-view", ColdRequest::View);
}

#[test]
fn distinct_paged_portals_yield_on_cold_reads_and_read_each_object_once() {
    cold_result_stream("result-stream-cold-distinct", ColdRequest::Distinct);
}

#[test]
fn paged_joins_stream_cold_tables() {
    cold_result_stream("result-stream-cold-join", ColdRequest::Join);
}

#[test]
fn joins_stream_cold_tables() {
    cold_result_stream("result-stream-cold-join-query", ColdRequest::JoinQuery);
}

#[test]
fn paged_right_joins_stream_cold_tables() {
    cold_result_stream("result-stream-cold-right-join", ColdRequest::RightJoin);
}

#[test]
fn right_joins_stream_cold_tables() {
    cold_result_stream(
        "result-stream-cold-right-join-query",
        ColdRequest::RightJoinQuery,
    );
}

#[test]
fn groupings_open_their_stream_before_running() {
    cold_result_stream(
        "result-stream-cold-grouped-query",
        ColdRequest::GroupedQuery,
    );
}

#[test]
fn spooled_joins_write_external_runs_to_object_storage() {
    cold_result_stream("result-stream-cold-spooled-join", ColdRequest::SpooledJoin);
}

#[test]
fn ctes_read_twice_merge_and_stream() {
    cold_result_stream("result-stream-cold-cte-twice", ColdRequest::CteTwice);
}

#[test]
fn derived_self_joins_merge_and_stream() {
    cold_result_stream(
        "result-stream-cold-derived-self",
        ColdRequest::DerivedSelfJoin,
    );
}

fn cold_result_stream(name: &str, request: ColdRequest) {
    let paged = matches!(
        request,
        ColdRequest::Paged
            | ColdRequest::Grouped
            | ColdRequest::Distinct
            | ColdRequest::Union
            | ColdRequest::Join
            | ColdRequest::RightJoin
    );
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
    let _namespace = crate::object_store::sim::open_namespace(&config.object_store_bucket, 229);
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE cold_orders(id integer, code text)",
        false,
    );
    session.success(
        &mut engine,
        "CREATE VIEW cold_view AS SELECT id AS ident, code FROM cold_orders WHERE id > 0",
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
    // The object service runs on its own thread, as a real one would: a
    // spooled result's external runs write synchronously.
    let mut network = config.clone();
    let objects = HttpObjectServer::start(&mut network);
    replace_uncached_blocks(&mut engine, &network);

    let mut conn = Conn::new(&config, &mut budget).unwrap();
    let (mut client, server) = socket_pair();
    conn.open(server, 1).unwrap();
    conn.enter_ready_for_tests(&engine);
    let auth = AuthContext {
        mode: crate::pg::auth::AuthMode::Trust,
        password: String::new(),
        scram: None,
    };
    let sql = "SELECT id, code FROM cold_orders";
    client
        .write_all(&match request {
            ColdRequest::Select => query(sql),
            ColdRequest::UnionQuery => query(
                "SELECT id, code FROM cold_orders WHERE id <= 160 UNION ALL \
                 SELECT id, code FROM cold_orders WHERE id > 160",
            ),
            ColdRequest::Derived => {
                query("SELECT id, code FROM (SELECT id, code FROM cold_orders) s")
            }
            ColdRequest::Cte => query(
                "WITH c AS NOT MATERIALIZED (SELECT id, code FROM cold_orders) \
                 SELECT id, code FROM c",
            ),
            ColdRequest::View => query("SELECT ident, code FROM cold_view"),
            ColdRequest::Union => paged_request(
                "SELECT id, code FROM cold_orders WHERE id <= 160 UNION ALL \
                 SELECT id, code FROM cold_orders WHERE id > 160",
                50,
            ),
            ColdRequest::Paged => paged_request(sql, 50),
            ColdRequest::RightJoinQuery => query(
                "SELECT b.id, b.code FROM cold_orders a RIGHT JOIN cold_orders b ON a.id = b.id",
            ),
            ColdRequest::SpooledJoin => query(
                "SELECT a.id, a.code FROM cold_orders a \
                 JOIN (SELECT id FROM cold_orders OFFSET 0) b ON a.id = b.id",
            ),
            ColdRequest::CteTwice => query(
                "WITH c AS NOT MATERIALIZED (SELECT id, code FROM cold_orders) \
                 SELECT a.id, a.code FROM c a JOIN c b ON a.id = b.id",
            ),
            ColdRequest::DerivedSelfJoin => query(
                "SELECT a.id, a.code FROM (SELECT id, code FROM cold_orders) a \
                 JOIN (SELECT id FROM cold_orders) b ON a.id = b.id",
            ),
            ColdRequest::GroupedQuery => {
                query("SELECT id, max(code), count(*) FROM cold_orders GROUP BY id")
            }
            ColdRequest::Join => paged_request(
                "SELECT a.id, a.code FROM cold_orders a JOIN cold_orders b ON a.id = b.id",
                50,
            ),
            ColdRequest::JoinQuery => {
                query("SELECT a.id, a.code FROM cold_orders a JOIN cold_orders b ON a.id = b.id")
            }
            ColdRequest::RightJoin => paged_request(
                "SELECT b.id, b.code FROM cold_orders a RIGHT JOIN cold_orders b ON a.id = b.id",
                50,
            ),
            ColdRequest::Copy => query("COPY cold_orders TO STDOUT"),
            ColdRequest::Distinct => paged_request("SELECT DISTINCT id, code FROM cold_orders", 50),
            ColdRequest::Grouped => paged_request(
                "SELECT id, max(code), count(*) FROM cold_orders GROUP BY id",
                50,
            ),
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
    // The second query of a UNION ALL starts its own cold scan.
    let mut second_query_yielded = false;
    let mut gets = 0usize;
    let mut deadline = Instant::now() + Duration::from_secs(10);
    loop {
        read_available(&mut client, &mut received);
        let delivered = messages(&received)
            .iter()
            .filter(|message| matches!(message.0, b'D' | b'd'))
            .count();
        if messages(&received).iter().any(|message| message.0 == b'Z') {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cold result stream did not finish"
        );
        // A grouping reads its whole input before its first row: its waits
        // come before any row.
        let grouped = matches!(
            request,
            ColdRequest::Grouped | ColdRequest::Distinct | ColdRequest::GroupedQuery
        );
        if conn.parked_wait() == Some(StatementWait::ObjectIo)
            && conn.result_stream_active()
            && (grouped || !paged || delivered > 50)
            && (grouped || delivered > 0)
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
        if conn.parked_wait() == Some(StatementWait::ObjectIo)
            && conn.result_stream_active()
            && delivered >= 160
        {
            second_query_yielded = true;
        }
        let served = objects.requests.load(std::sync::atomic::Ordering::Relaxed);
        if served != gets {
            gets = served;
            deadline = Instant::now() + Duration::from_secs(10);
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
    let fetched = objects.fetched.lock().unwrap().len();
    let response = messages(&received);
    let kinds: Vec<u8> = response.iter().map(|message| message.0).collect();
    assert_eq!(
        &kinds[kinds.len() - 2..],
        b"CZ",
        "{:?}",
        String::from_utf8_lossy(response[kinds.len() - 2].1)
    );
    match request {
        ColdRequest::Grouped | ColdRequest::Distinct => {
            assert_eq!(kinds.iter().filter(|kind| **kind == b's').count(), 3);
            assert_eq!(response[kinds.len() - 2].1, b"SELECT 170\0");
            // The portal opens its stream before running, and the input read
            // position survives each wait. Its one repeated GET matches an
            // ungrouped paged stream of the same table; a restarted
            // statement would read every object again.
            assert!(gets <= fetched + 1, "{gets} GETs of {}", fetched);
        }
        ColdRequest::Paged | ColdRequest::Union | ColdRequest::Join | ColdRequest::RightJoin => {
            // Each full page suspends; the unlimited Execute returns the rest.
            assert_eq!(kinds.iter().filter(|kind| **kind == b's').count(), 3);
            assert_eq!(response[kinds.len() - 2].1, b"SELECT 170\0");
        }
        ColdRequest::Select
        | ColdRequest::Derived
        | ColdRequest::Cte
        | ColdRequest::View
        | ColdRequest::JoinQuery
        | ColdRequest::RightJoinQuery
        | ColdRequest::GroupedQuery
        | ColdRequest::SpooledJoin
        | ColdRequest::CteTwice
        | ColdRequest::DerivedSelfJoin => {
            assert_eq!(kinds.iter().filter(|kind| **kind == b'T').count(), 1);
            assert_eq!(response[kinds.len() - 2].1, b"SELECT 320\0");
        }
        ColdRequest::UnionQuery => {
            assert_eq!(kinds.iter().filter(|kind| **kind == b'T').count(), 1);
            assert_eq!(response[kinds.len() - 2].1, b"SELECT 320\0");
            // Each query reads the table once, in the stream; an attempt
            // run before it would have read every object a second time.
            assert!(gets <= 2 * fetched + 2, "{gets} GETs of {}", fetched);
        }
        ColdRequest::Copy => {
            assert_eq!(kinds[0], b'H');
            assert_eq!(response[kinds.len() - 2].1, b"COPY 320\0");
            let lines: Vec<_> = response
                .iter()
                .filter(|message| message.0 == b'd')
                .collect();
            assert_eq!(lines.len(), 320);
            for (index, line) in lines.iter().enumerate() {
                let text = core::str::from_utf8(line.1).unwrap().trim_end_matches('\n');
                let (id, code) = text.split_once('\t').unwrap();
                assert_eq!(id, (index + 1).to_string());
                assert_eq!(checksum(code.as_bytes()), expected[index]);
            }
        }
    }
    let rows: Vec<_> = response
        .iter()
        .filter(|message| message.0 == b'D')
        .collect();
    assert_eq!(
        rows.len(),
        if request == ColdRequest::Copy { 0 } else { 320 }
    );
    for (index, row) in rows.iter().enumerate() {
        let values = fields(row.1);
        assert_eq!(values[0], (index + 1).to_string().as_bytes());
        assert_eq!(checksum(values[1]), expected[index]);
        if matches!(request, ColdRequest::Grouped | ColdRequest::GroupedQuery) {
            assert_eq!(values[2], b"1");
        }
    }
    let metrics = engine.result_stream_metrics();
    if request == ColdRequest::SpooledJoin {
        // It ran in the general executor, which wrote its derived table's run.
        assert_eq!(metrics.opened, 0);
        assert!(objects.uploads.load(std::sync::atomic::Ordering::Relaxed) > 0);
    } else {
        assert!(
            yielded_after_rows,
            "a cold read after delivered rows, or a grouping's input read, must yield instead of blocking"
        );
        // Every shape here streams; one that silently spooled would read
        // and hold its whole result first.
        assert_eq!(metrics.opened, 1);
        assert_eq!(
            metrics.spooled_shape + metrics.spooled_capacity + metrics.spooled_effects,
            0
        );
    }
    if matches!(request, ColdRequest::Union | ColdRequest::UnionQuery) {
        assert!(second_query_yielded, "the second query's cold scan yields");
    }
    drop(conn.close());
    drop((other, engine, objects));
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
}
