//! An index path's candidate cursor resumes the row a storage wait
//! interrupted: every visible candidate is returned once, in candidate order,
//! whether its row is resident or saved in object storage.

use super::object_reads::{advance, read_request, replace_uncached_blocks, reply};
use super::*;
use crate::sql::types::ColType;
use crate::storage::{CandidateCursor, Candidates, rowenc};
use std::net::TcpListener;
use std::time::Instant;

const ROWS: i32 = 240;

fn decoded_id(bytes: &[u8]) -> i32 {
    let mut values = [Datum::Null; 2];
    rowenc::decode(bytes, &[ColType::Int4, ColType::Text], &mut values).unwrap();
    match values[0] {
        Datum::Int4(id) => id,
        other => panic!("id column: {other:?}"),
    }
}

/// Reads every candidate, serving each object GET by hand; returns the ids
/// in the order the cursor returned them and the waits after the first row.
fn read_all(
    engine: &mut Engine,
    table: usize,
    candidates: &[u64],
    listener: Option<&TcpListener>,
    prefix: &str,
    namespace: Option<&crate::object_store::sim::SimNamespaceHandle>,
) -> (Vec<i32>, usize) {
    let mut budget = Budget::new(1 << 20);
    let output = Arena::new(&mut budget, "candidate rows", 1 << 20).unwrap();
    let mut cursor = {
        let _snapshot = engine
            .storage
            .nested_statement_snapshot(engine.storage.lsn());
        CandidateCursor::open(&engine.storage, table, 0)
    };
    let owner = ExecutionIdentity::new(41, engine.execution_identity().database_oid).read_owner();
    let mut ids = Vec::new();
    let mut waits = 0;
    let mut deadline = crate::test_wait::deadline();
    loop {
        advance(engine);
        let used = output.used();
        let prior = engine.replace_read_owner(Some(owner));
        let result = crate::mem::guard::forbid_alloc(|| {
            cursor.next(&engine.storage, &output, Candidates::Rowids(candidates))
        });
        let _ = engine.replace_read_owner(prior);
        match result {
            Ok(Some(row)) => {
                ids.push(decoded_id(row.bytes));
                deadline = crate::test_wait::deadline();
            }
            Ok(None) => break,
            Err(error) => {
                assert_eq!(error.sqlstate, sqlstate::INTERNAL_IO_WAIT);
                // A wait releases what the interrupted read allocated.
                assert_eq!(output.used(), used);
                waits += usize::from(!ids.is_empty());
                let (listener, namespace) = (listener.unwrap(), namespace.unwrap());
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let request = read_request(engine, &mut stream);
                        let key = request
                            .lines()
                            .next()
                            .unwrap()
                            .split_whitespace()
                            .nth(1)
                            .unwrap()
                            .strip_prefix(prefix)
                            .unwrap()
                            .to_owned();
                        let body = namespace.borrow().object_bytes(&key).unwrap().to_vec();
                        reply(&mut stream, &request, &body);
                        deadline = crate::test_wait::deadline();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "candidate read did not advance");
                        std::thread::yield_now();
                    }
                    Err(error) => panic!("candidate read accept: {error}"),
                }
            }
        }
    }
    engine.ckpt.as_mut().unwrap().cancel_read_owner(owner);
    (ids, waits)
}

#[test]
fn candidate_cursor_resumes_waits_mid_scan() {
    let label = "candidate-cursor-waits";
    let mut config = test_config(label);
    config.object_store_on = true;
    config.object_store_sim = true;
    config.object_store_bucket = format!("{label}-{}", std::process::id());
    config.wal_upload = true;
    config.wal_upload_sync = true;
    config.object_store_get_slots = 4;
    config.object_store_hedge_after_ms = 0;
    config.block_cache_bytes = 0;
    config.disk_cache_bytes = 0;
    config.max_tables = 4;
    config.row_overlay_slots = 512;
    config.max_spill_generations_per_table = 2;
    config.memtable_bytes = 8 << 20;
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    let namespace = crate::object_store::sim::open_namespace(&config.object_store_bucket, 227);
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        &format!(
            "CREATE TABLE candidates(id integer, pad text); \
             INSERT INTO candidates SELECT g, repeat(md5(g::text), 32) \
             FROM generate_series(1, {ROWS}) g; \
             DELETE FROM candidates WHERE id % 7 = 0"
        ),
        false,
    );
    let table = engine.storage.find_table("public", "candidates").unwrap();
    // Every row identity, deleted rows among them, newest first: an index
    // scan read backward.
    let mut rowids = Vec::new();
    engine
        .storage
        .for_each_row_state(table, &mut |rowid, _| {
            rowids.push(rowid);
            Ok(core::ops::ControlFlow::Continue(()))
        })
        .unwrap();
    rowids.sort_unstable_by(|a, b| b.cmp(a));
    let expected: Vec<i32> = (1..=ROWS).rev().filter(|id| id % 7 != 0).collect();
    // Resident rows, before any is saved.
    let (resident, _) = read_all(&mut engine, table, &rowids, None, "", None);
    assert_eq!(resident, expected);
    assert!(engine.checkpoint().unwrap());
    drop((session, engine));
    std::fs::remove_dir_all(&config.data_dir).unwrap();

    // Saved rows, each block read from object storage with a wait.
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut network = config.clone();
    network.object_store_sim = false;
    network.object_store_endpoint = listener.local_addr().unwrap().to_string();
    network.object_store_access_key = "test-access".to_owned();
    network.object_store_secret_key = "test-secret".to_owned();
    replace_uncached_blocks(&mut engine, &network);
    let prefix = format!("/{}/", config.object_store_bucket);
    let (saved, waits) = read_all(
        &mut engine,
        table,
        &rowids,
        Some(&listener),
        &prefix,
        Some(&namespace),
    );
    assert_eq!(saved, expected);
    assert!(waits > 0, "the scan must wait after returning a prefix");
    assert!(!engine.block_reads_pending());
    replace_uncached_blocks(&mut engine, &config);
    drop(engine);
    std::fs::remove_dir_all(&config.data_dir).unwrap();
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
}
