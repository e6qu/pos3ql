//! A join's scan node resumes after storage waits: while it finds an
//! execution's candidates and while it reads a row. Every row passing the
//! scan's condition is returned once, in the path's order, for each
//! execution, whether the table's rows are resident or saved in object
//! storage. A retried open finds the blocks the interrupted one fetched in
//! the block cache, as a server's does, so it gets further each time.

use super::object_reads::{advance, read_request, replace_blocks, replace_uncached_blocks, reply};
use super::*;
use crate::sql::query::PathScan;
use crate::sql::query::index_paths::{IndexPages, ScanKind, ScanPath, SortKind};
use std::net::TcpListener;
use std::time::Instant;

const ROWS: i32 = 240;
/// Room for every block the table's saved rows span.
const CACHE_BYTES: usize = 4 << 20;

/// The path `kind` with nothing else known about it, as a join's plan
/// describes a sequential or bitmap heap scan to its node.
fn path(kind: ScanKind) -> ScanPath<'static> {
    ScanPath {
        kind,
        index_oid: 0,
        index_name: crate::util::StackStr::new(),
        index_pages: IndexPages::NONE,
        keys: &[],
        conditions: &[],
        predicate: &[],
        bitmap: &[],
        recheck: &[],
        absorbed: &[],
        rows: 0.0,
        width: 0,
        startup_cost: 0.0,
        total_cost: 0.0,
        backward: false,
        sort: SortKind::None,
        deduplicated: false,
        stop_after: None,
        single_row: false,
        searches_unmodeled: false,
        sort_cost: None,
        limit_cost: None,
        grouped: None,
    }
}

/// The ids every execution of the node returns, one execution per bound
/// in `bounds`, serving each object GET by hand; and the waits after an
/// execution's first row.
fn run(
    engine: &mut Engine,
    kind: ScanKind,
    bounds: &[i32],
    object: Option<(
        &TcpListener,
        &str,
        &crate::object_store::sim::SimNamespaceHandle,
    )>,
) -> (Vec<Vec<i32>>, usize) {
    let mut budget = Budget::new(16 << 20);
    let arena = Arena::new(&mut budget, "path scan rows", 16 << 20).unwrap();
    let select =
        crate::sql::parser::parse_query("SELECT * FROM candidates WHERE id <= $1", &arena).unwrap();
    let from = select.from.as_ref().unwrap();
    let owner = ExecutionIdentity::new(41, engine.execution_identity().database_oid).read_owner();
    // The node borrows the storage for the statement, as a plan does while
    // the session that owns the engine serves its reads between calls.
    // SAFETY: the engine is neither moved nor dropped, and its storage is
    // not replaced, while the node lives.
    let storage: &crate::storage::Storage = unsafe { &*core::ptr::from_ref(&engine.storage) };
    let params = [Datum::Int4(bounds[0])];
    let mut node = {
        let _snapshot = storage.nested_statement_snapshot(storage.lsn());
        PathScan::new(
            storage,
            0,
            from,
            select.where_clause,
            path(kind),
            &arena,
            &params,
            &crate::sql::eval::NO_HOOKS,
        )
        .unwrap()
    };
    let mut executions = Vec::new();
    let mut waits = 0;
    for &bound in bounds {
        let params = [Datum::Int4(bound)];
        node.rescan();
        let mut ids = Vec::new();
        let mut deadline = crate::test_wait::deadline();
        // The arena's use at the last wait since a row was returned, and
        // whether the execution's open has succeeded.
        let mut waited_at = None;
        let mut opened = false;
        loop {
            advance(engine);
            let prior = engine.replace_read_owner(Some(owner));
            let result = {
                let _snapshot = storage.nested_statement_snapshot(storage.lsn());
                crate::mem::guard::forbid_alloc(|| {
                    node.next(&arena, &params, &crate::sql::eval::NO_HOOKS, None)
                })
            };
            let _ = engine.replace_read_owner(prior);
            match result {
                Ok(true) => {
                    waited_at = None;
                    opened = true;
                    match node.values()[0] {
                        Datum::Int4(id) => ids.push(id),
                        other => panic!("id column: {other:?}"),
                    }
                    deadline = crate::test_wait::deadline();
                }
                Ok(false) => break,
                Err(error) => {
                    assert_eq!(error.sqlstate, sqlstate::INTERNAL_IO_WAIT, "{error:?}");
                    // A wait releases what the interrupted call allocated:
                    // retries hold no more than the first one did, except
                    // for the storage an open that succeeded keeps before
                    // its first read waits.
                    let used = arena.used();
                    if let Some(first) = waited_at
                        && first != used
                    {
                        assert!(
                            !opened,
                            "a {kind:?} wait after {} rows of bound {bound} grew the arena from {first} to {used}",
                            ids.len()
                        );
                        opened = true;
                    }
                    waited_at = Some(used);
                    waits += usize::from(!ids.is_empty());
                    let (listener, prefix, namespace) = object.expect("resident rows never wait");
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
                            assert!(Instant::now() < deadline, "the scan did not advance");
                            std::thread::yield_now();
                        }
                        Err(error) => panic!("path scan accept: {error}"),
                    }
                }
            }
        }
        executions.push(ids);
    }
    engine.ckpt.as_mut().unwrap().cancel_read_owner(owner);
    (executions, waits)
}

#[test]
fn path_scan_resumes_waits_across_rescans() {
    let label = "path-scan-waits";
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
    let namespace = crate::object_store::sim::open_namespace(&config.object_store_bucket, 229);
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        &format!(
            "CREATE TABLE candidates(id integer, pad text); \
             CREATE INDEX candidates_id ON candidates(id); \
             INSERT INTO candidates SELECT g, (SELECT string_agg(md5(g::text || i::text), '') FROM generate_series(1, 96) i) \
             FROM generate_series(1, {ROWS}) g; \
             DELETE FROM candidates WHERE id % 7 = 0"
        ),
        false,
    );
    // Each execution's bound, a rescan with a smaller, larger and equal one.
    let bounds = [200, 90, ROWS, 90];
    let expected: Vec<Vec<i32>> = bounds
        .iter()
        .map(|&bound| (1..=bound).filter(|id| id % 7 != 0).collect())
        .collect();
    for kind in [ScanKind::Seq, ScanKind::Bitmap] {
        let (resident, _) = run(&mut engine, kind, &bounds, None);
        assert_eq!(resident, expected);
    }
    assert!(engine.checkpoint().unwrap());
    drop((session, engine));
    std::fs::remove_dir_all(&config.data_dir).unwrap();

    // Saved rows, each block read from object storage with a wait and kept
    // in a RAM cache. The rows span several blocks, so a sequential scan
    // waits again after its first rows.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut network = config.clone();
    network.object_store_sim = false;
    network.object_store_endpoint = listener.local_addr().unwrap().to_string();
    network.object_store_access_key = "test-access".to_owned();
    network.object_store_secret_key = "test-secret".to_owned();
    let prefix = format!("/{}/", config.object_store_bucket);
    for kind in [ScanKind::Seq, ScanKind::Bitmap] {
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        replace_blocks(&mut engine, &network, CACHE_BYTES);
        let (saved, waits) = run(
            &mut engine,
            kind,
            &bounds,
            Some((&listener, &prefix, &namespace)),
        );
        assert_eq!(saved, expected);
        // A sequential scan reads each block as it reaches it.
        assert!(
            kind != ScanKind::Seq || waits > 0,
            "the scan must wait after returning a prefix"
        );
        assert!(!engine.block_reads_pending());
        replace_uncached_blocks(&mut engine, &config);
        drop(engine);
        std::fs::remove_dir_all(&config.data_dir).unwrap();
    }
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
}
