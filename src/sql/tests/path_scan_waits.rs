//! A join's scan node resumes after storage waits: while it finds an
//! execution's candidates and while it reads a row. Every row passing the
//! scan's condition is returned once, in the path's order, for each
//! execution, whether the table's rows are resident or saved in object
//! storage, and without a block cache.
//!
//! The test drives the node as a statement does: each call after a wait is
//! a restarted attempt of the statement's reads and each returned row is
//! progress, so a retried open that would download a block a third time
//! reads the rest synchronously and finishes. An object store answers every
//! GET on its own thread, as a real one does.

use super::object_reads::{replace_uncached_blocks, reply};
use super::*;
use crate::sql::query::PathScan;
use crate::sql::query::index_paths::{IndexPages, ScanKind, ScanPath, SortKind};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Rows of about 3 KB, so the saved rows span two 256 KiB blocks and a
/// sequential scan waits again after its first rows.
const ROWS: i32 = 100;

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

/// Moves the checkpointer's pending block reads on.
pub(super) fn advance_block_reads(checkpointer: &mut crate::checkpoint::Checkpointer) {
    checkpointer.advance_background_io(Instant::now());
    for slot in 0..checkpointer.block_read_slots() {
        if checkpointer.pending_block_read_fd(slot).is_some() {
            crate::mem::guard::forbid_alloc(|| checkpointer.advance_pending_block_read(slot))
                .unwrap();
        }
    }
}

/// Answers every object GET on `listener` from `namespace` until `stop`,
/// each connection on its own thread: a client may connect before it sends
/// its request.
pub(super) fn serve(
    listener: TcpListener,
    prefix: String,
    namespace: crate::object_store::sim::SimNamespaceHandle,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut connections = Vec::new();
        while !stop.load(Ordering::Acquire) {
            let mut stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    continue;
                }
                Err(error) => panic!("object GET accept: {error}"),
            };
            let (prefix, namespace, stop) = (prefix.clone(), namespace.clone(), Arc::clone(&stop));
            connections.push(std::thread::spawn(move || {
                let mut head = [0; 8192];
                let Some((_, len)) =
                    crate::test_wait::read_request_head(&mut stream, &mut head, &stop)
                else {
                    return;
                };
                let request = core::str::from_utf8(&head[..len]).unwrap().to_owned();
                let key = request
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .strip_prefix(prefix.as_str())
                    .unwrap()
                    .to_owned();
                let body = namespace.borrow().object_bytes(&key).unwrap().to_vec();
                reply(&mut stream, &request, &body);
            }));
        }
        for connection in connections {
            connection.join().unwrap();
        }
    })
}

/// The ids every execution of the node returns, one execution per bound
/// in `bounds`, and the waits after an execution's first row.
fn run(engine: &mut Engine, kind: ScanKind, bounds: &[i32]) -> (Vec<Vec<i32>>, usize) {
    let mut budget = Budget::new(4 << 20);
    let arena = Arena::new(&mut budget, "path scan rows", 4 << 20).unwrap();
    let select =
        crate::sql::parser::parse_query("SELECT * FROM candidates WHERE id <= $1", &arena).unwrap();
    let from = select.from.as_ref().unwrap();
    let owner = ExecutionIdentity::new(41, engine.execution_identity().database_oid).read_owner();
    let prior_owner = engine.read_owner;
    // The node borrows the storage for the statement while the checkpointer
    // serves its reads between calls, as a session's engine does.
    let storage = &engine.storage;
    let checkpointer = engine.ckpt.as_mut().unwrap();
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
    // Rows returned so far: the statement's progress across waits.
    let mut progress = 0;
    let mut restarted = false;
    checkpointer
        .begin_statement_reads(owner, false, progress)
        .unwrap();
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
            advance_block_reads(checkpointer);
            if restarted {
                checkpointer
                    .begin_statement_reads(owner, true, progress)
                    .unwrap();
            }
            checkpointer.set_read_owner(Some(owner));
            let result = {
                let _snapshot = storage.nested_statement_snapshot(storage.lsn());
                crate::mem::guard::forbid_alloc(|| {
                    node.next(
                        storage,
                        crate::storage::CursorScratch::Statement,
                        &arena,
                        &params,
                        &crate::sql::eval::NO_HOOKS,
                        None,
                    )
                })
            };
            checkpointer.set_read_owner(prior_owner);
            restarted = result.is_err();
            match result {
                Ok(true) => {
                    waited_at = None;
                    opened = true;
                    progress += 1;
                    checkpointer.statement_progressed(owner);
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
                    assert!(Instant::now() < deadline, "the scan did not advance");
                    std::thread::yield_now();
                }
            }
        }
        executions.push(ids);
    }
    drop(node);
    checkpointer.forget_statement_reads(owner);
    checkpointer.cancel_read_owner(owner);
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
             INSERT INTO candidates SELECT g, \
             (SELECT string_agg(md5(g::text || i::text), '') FROM generate_series(1, 96) i) \
             FROM generate_series(1, {ROWS}) g; \
             DELETE FROM candidates WHERE id % 7 = 0"
        ),
        false,
    );
    // A smaller, then a larger rescan: the larger one outgrows the node's
    // candidate storage.
    let bounds = [70, ROWS, 30];
    let expected: Vec<Vec<i32>> = bounds
        .iter()
        .map(|&bound| (1..=bound).filter(|id| id % 7 != 0).collect())
        .collect();
    for kind in [ScanKind::Seq, ScanKind::Bitmap] {
        let (resident, waits) = run(&mut engine, kind, &bounds);
        assert_eq!(waits, 0, "resident rows never wait");
        assert_eq!(resident, expected);
    }
    assert!(engine.checkpoint().unwrap());
    drop((session, engine));
    std::fs::remove_dir_all(&config.data_dir).unwrap();

    // Saved rows, each block read from object storage with a wait, with no
    // block cache to keep them: a retried open downloads its blocks again.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut network = config.clone();
    network.object_store_sim = false;
    network.object_store_endpoint = listener.local_addr().unwrap().to_string();
    network.object_store_access_key = "test-access".to_owned();
    network.object_store_secret_key = "test-secret".to_owned();
    let stop = Arc::new(AtomicBool::new(false));
    let server = serve(
        listener,
        format!("/{}/", config.object_store_bucket),
        namespace.clone(),
        Arc::clone(&stop),
    );
    for kind in [ScanKind::Seq, ScanKind::Bitmap] {
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        replace_uncached_blocks(&mut engine, &network);
        let (saved, waits) = run(&mut engine, kind, &bounds);
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
    stop.store(true, Ordering::Release);
    server.join().unwrap();
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
}
