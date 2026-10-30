use super::object_reads::{accept, advance, read_request, replace_uncached_blocks, reply};
use super::*;
use crate::storage::{RowCursor, RowCursorBuffers, SpilledRowRepresentation};
use crate::store::{BlockStore, StoreError};
use std::net::TcpListener;
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
enum SavedRows {
    Columns,
    Overflow,
}

fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325u64, |state, byte| {
        (state ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

fn object_key<'a>(request: &'a str, prefix: &str) -> &'a str {
    request
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .strip_prefix(prefix)
        .unwrap()
}

fn held_query_read(mode: crate::config::TransactionMode, saved: SavedRows, cancel: bool) {
    let kind = match saved {
        SavedRows::Columns => "columns",
        SavedRows::Overflow => "overflow",
    };
    let label = format!("query-held-{mode:?}-{kind}-{cancel}").to_lowercase();
    let mut config = test_config(&label);
    config.transaction_mode = mode;
    config.object_store_on = true;
    config.object_store_sim = true;
    config.object_store_bucket = format!("{label}-{}", std::process::id());
    config.wal_upload = true;
    config.wal_upload_sync = true;
    // Two demanded columns, one lookahead and an independent client's read.
    config.object_store_get_slots = 4;
    config.object_store_hedge_after_ms = 0;
    config.block_cache_bytes = 0;
    config.disk_cache_bytes = 0;
    config.max_tables = 4;
    config.table_rows = 512;
    config.max_spill_generations_per_table = 2;
    config.memtable_bytes = 8 << 20;
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    let namespace = crate::object_store::sim::open_namespace(&config.object_store_bucket, 227);
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE saved_input(id integer, code text)",
        false,
    );
    let (rows, width) = match saved {
        SavedRows::Columns => (320, 1024),
        SavedRows::Overflow => (3, 320_000),
    };
    let mut insert = String::from("INSERT INTO saved_input VALUES ");
    let mut expected = Vec::with_capacity(rows);
    for id in 1..=rows {
        if id != 1 {
            insert.push(',');
        }
        insert.push_str(&format!("({id},'"));
        let start = insert.len();
        let mut random = id as u64;
        for _ in 0..width {
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
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE cached_input(id integer); INSERT INTO cached_input VALUES(42)",
        false,
    );
    let table = engine.storage.find_table("public", "saved_input").unwrap();
    let foreground = engine
        .storage
        .table(table)
        .spill_generations()
        .handle(0)
        .unwrap()
        .filter;
    let mut foreground_hash = [0; 64];
    foreground.write_key(&mut foreground_hash);
    let foreground_key = format!(
        "{}blocks/{}",
        config.object_store_prefix,
        core::str::from_utf8(&foreground_hash).unwrap()
    );
    let state_bytes =
        RowCursor::position_bytes(config.table_rows, config.max_spill_generations_per_table)
            .unwrap();
    let mut reader_budget =
        Budget::new(RowCursorBuffers::allocation_bytes(&config) + state_bytes + (1 << 20));
    let mut cursor = RowCursor::closed();
    let mut buffers = RowCursorBuffers::new(&config, &mut reader_budget).unwrap();
    let state = Arena::new(&mut reader_budget, "held query positions", state_bytes).unwrap();
    let mut output = Arena::new(&mut reader_budget, "held query row", 1 << 20).unwrap();
    assert_eq!(reader_budget.remaining(), 0);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut network = config.clone();
    network.object_store_sim = false;
    network.object_store_endpoint = listener.local_addr().unwrap().to_string();
    network.object_store_access_key = "test-access".to_owned();
    network.object_store_secret_key = "test-secret".to_owned();
    replace_uncached_blocks(&mut engine, &network);
    {
        let _snapshot = engine
            .storage
            .nested_statement_snapshot(engine.storage.lsn());
        crate::mem::guard::forbid_alloc(|| cursor.open(&engine.storage, table, 0, &state)).unwrap();
    }
    assert!(
        !engine.block_reads_pending(),
        "capturing a query source must not fetch its rows"
    );
    let blocks = engine.ckpt.as_ref().unwrap().block_stack();
    let prefix = format!("/{}/", config.object_store_bucket);
    let query_owner =
        ExecutionIdentity::new(31, engine.execution_identity().database_oid).read_owner();
    let other_owner =
        ExecutionIdentity::new(32, engine.execution_identity().database_oid).read_owner();
    let mut count = 0;
    let mut tested_wait = false;
    let mut cold_read_before_first_row = false;
    let mut deadline = Instant::now() + Duration::from_secs(2);
    loop {
        advance(&mut engine);
        let prior = engine.replace_read_owner(Some(query_owner));
        let result =
            crate::mem::guard::forbid_alloc(|| cursor.next(&mut buffers, &state, &output, None));
        let _ = engine.replace_read_owner(prior);
        match result {
            Ok(Some(row)) => {
                let mut decoded = [Datum::Null; 2];
                let values = match row.representation {
                    SpilledRowRepresentation::Encoded(bytes) => {
                        crate::mem::guard::forbid_alloc(|| {
                            crate::storage::rowenc::decode(
                                bytes,
                                &[ColType::Int4, ColType::Text],
                                &mut decoded,
                            )
                        })
                        .unwrap();
                        &decoded[..]
                    }
                    SpilledRowRepresentation::Values(values) => values,
                };
                assert_eq!(values[0], Datum::Int4((count + 1) as i32));
                let Datum::Text(code) = values[1] else {
                    panic!("text source column")
                };
                assert_eq!(code.len(), width);
                assert_eq!(checksum(code.as_bytes()), expected[count]);
                count += 1;
                output.reset();
                deadline = Instant::now() + Duration::from_secs(2);
            }
            Ok(None) => break,
            Err(error) => {
                assert_eq!(error.sqlstate, sqlstate::INTERNAL_IO_WAIT);
                cold_read_before_first_row |= count == 0;
                let (mut stream, request) = match listener.accept() {
                    Ok((mut stream, _)) => {
                        let request = read_request(&mut engine, &mut stream);
                        (stream, request)
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "query input did not advance");
                        std::thread::yield_now();
                        continue;
                    }
                    Err(error) => panic!("query read accept: {error}"),
                };
                let body = namespace
                    .borrow()
                    .object_bytes(object_key(&request, &prefix))
                    .unwrap()
                    .to_vec();
                if count != 0 && count < rows && !tested_wait {
                    tested_wait = true;
                    let mut held = vec![(stream, request, body)];
                    let mut foreground_bytes = [0; crate::store::MAX_PAYLOAD];
                    let prior = engine.replace_read_owner(Some(other_owner));
                    let started = crate::mem::guard::forbid_alloc(|| {
                        blocks
                            .lock()
                            .unwrap()
                            .get(&foreground, &mut foreground_bytes)
                    });
                    let _ = engine.replace_read_owner(prior);
                    assert_eq!(started, Err(StoreError::NotReady));
                    let (mut foreground_stream, foreground_request) = loop {
                        let (stream, request) = accept(&mut engine, &listener);
                        if object_key(&request, &prefix) == foreground_key {
                            break (stream, request);
                        }
                        let body = namespace
                            .borrow()
                            .object_bytes(object_key(&request, &prefix))
                            .unwrap()
                            .to_vec();
                        held.push((stream, request, body));
                    };
                    let fds: Vec<_> = (0..config.object_store_get_slots)
                        .map(|slot| engine.pending_block_read_fd(slot))
                        .collect();
                    let state_used = state.used();
                    let output_used = output.used();
                    for _ in 0..3 {
                        let prior = engine.replace_read_owner(Some(query_owner));
                        let waiting = crate::mem::guard::forbid_alloc(|| {
                            cursor.next(&mut buffers, &state, &output, None)
                        });
                        let _ = engine.replace_read_owner(prior);
                        assert_eq!(waiting.err().unwrap().sqlstate, sqlstate::INTERNAL_IO_WAIT);
                        assert_eq!(state.used(), state_used);
                        assert_eq!(output.used(), output_used);
                        assert!(
                            (0..config.object_store_get_slots)
                                .all(|slot| engine.pending_block_read_fd(slot) == fds[slot])
                        );
                    }
                    assert_eq!(
                        data_rows(&session.success(
                            &mut engine,
                            "SELECT id FROM cached_input",
                            true
                        )),
                        ["42"]
                    );
                    session.success(
                        &mut engine,
                        "BEGIN; INSERT INTO cached_input VALUES(43)",
                        true,
                    );
                    session.success(&mut engine, "COMMIT", false);
                    assert_eq!(
                        data_rows(&session.success(
                            &mut engine,
                            "SELECT count(*),sum(id) FROM cached_input",
                            true
                        )),
                        ["2|85"]
                    );
                    if cancel {
                        cursor.close();
                        engine.ckpt.as_mut().unwrap().cancel_read_owner(query_owner);
                        assert_eq!(
                            (0..config.object_store_get_slots)
                                .filter(|slot| engine.pending_block_read_fd(*slot).is_some())
                                .count(),
                            1
                        );
                        held.clear();
                    }
                    reply(
                        &mut foreground_stream,
                        &foreground_request,
                        namespace.borrow().object_bytes(&foreground_key).unwrap(),
                    );
                    let foreground_deadline = Instant::now() + Duration::from_secs(2);
                    loop {
                        advance(&mut engine);
                        let prior = engine.replace_read_owner(Some(other_owner));
                        let result = crate::mem::guard::forbid_alloc(|| {
                            blocks
                                .lock()
                                .unwrap()
                                .get(&foreground, &mut foreground_bytes)
                        });
                        let _ = engine.replace_read_owner(prior);
                        match result {
                            Ok(_) => break,
                            Err(StoreError::NotReady) => {}
                            Err(error) => panic!("independent read failed: {error:?}"),
                        }
                        assert!(
                            Instant::now() < foreground_deadline,
                            "independent read did not complete"
                        );
                        std::thread::yield_now();
                    }
                    if cancel {
                        break;
                    }
                    for (stream, request, body) in &mut held {
                        reply(stream, request, body);
                    }
                } else {
                    reply(&mut stream, &request, &body);
                }
                deadline = Instant::now() + Duration::from_secs(2);
            }
        }
    }
    assert!(
        cold_read_before_first_row,
        "saved input must be fetched from object storage"
    );
    assert!(
        tested_wait,
        "fixture must wait after returning a result prefix"
    );
    if cancel {
        assert!(count > 0 && count < rows);
    } else {
        assert_eq!(count, rows);
    }
    cursor.close();
    engine.ckpt.as_mut().unwrap().cancel_read_owner(query_owner);
    engine.ckpt.as_mut().unwrap().cancel_read_owner(other_owner);
    assert!(!engine.block_reads_pending());
    replace_uncached_blocks(&mut engine, &config);
    assert!(engine.checkpoint().unwrap());
    drop((session, engine, blocks));
    std::fs::remove_dir_all(&config.data_dir).unwrap();
    let mut cold_budget = test_engine_budget(&config, 1 << 29);
    let mut cold = Engine::new(&config, &mut cold_budget).unwrap();
    assert_eq!(
        data_rows(&run_with(
            &mut cold,
            &mut cold_budget,
            "SELECT count(*),sum(id) FROM saved_input; SELECT count(*),sum(id) FROM cached_input"
        )),
        [
            format!("{rows}|{}", rows * (rows + 1) / 2),
            "2|85".to_owned()
        ]
    );
    drop(cold);
    std::fs::remove_dir_all(&config.data_dir).unwrap();
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
}

#[test]
fn query_reads_held_mvcc_columns() {
    held_query_read(
        crate::config::TransactionMode::Mvcc,
        SavedRows::Columns,
        false,
    );
}
#[test]
fn query_reads_held_locking_columns() {
    held_query_read(
        crate::config::TransactionMode::Locking,
        SavedRows::Columns,
        false,
    );
}
#[test]
fn query_reads_held_mvcc_overflow() {
    held_query_read(
        crate::config::TransactionMode::Mvcc,
        SavedRows::Overflow,
        false,
    );
}
#[test]
fn query_reads_held_locking_overflow() {
    held_query_read(
        crate::config::TransactionMode::Locking,
        SavedRows::Overflow,
        false,
    );
}
#[test]
fn query_reads_held_mvcc_cancel() {
    held_query_read(
        crate::config::TransactionMode::Mvcc,
        SavedRows::Overflow,
        true,
    );
}
#[test]
fn query_reads_held_locking_cancel() {
    held_query_read(
        crate::config::TransactionMode::Locking,
        SavedRows::Overflow,
        true,
    );
}
