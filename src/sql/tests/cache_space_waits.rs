//! Writers that wait for cache space resume as soon as checkpoint cleanup
//! frees it, never behind unrelated object-store garbage collection.

use super::*;

fn durable_fixture(name: &str, overlay_slots: Option<usize>) -> (Engine, Budget, Config) {
    let mut config = test_config(name);
    config.object_store_on = true;
    config.object_store_sim = true;
    // One DELETE per paced beat: each beat stands for one round trip to a
    // slow object store, so collecting N orphans takes N beats.
    config.checkpoint_delete_objects_per_beat = 1;
    if let Some(slots) = overlay_slots {
        config.row_overlay_slots = slots;
    }
    config.object_store_bucket = format!("{name}-{}", std::process::id());
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
    let engine = Engine::new(&config, &mut budget).unwrap();
    (engine, budget, config)
}

fn run(
    engine: &mut Engine,
    session: &mut ConfiguredTransactionSession,
    sql: &str,
) -> ExecutionStatus {
    // A parked statement keeps its retained arena state, as the connection
    // keeps it across retries; only a new statement starts from an empty one.
    if session.transaction.suspended_statement_deadline().is_none() {
        session.arena.reset();
    }
    let mut responder = Responder::new(&mut session.send);
    engine
        .execute_simple_from(
            sql,
            0,
            &session.arena,
            &mut session.transaction,
            &mut session.pool,
            &mut session.cursors,
            &session.guc,
            &mut responder,
            session.connection_id,
            false,
        )
        .unwrap()
}

/// One reactor turn's maintenance: background uploads advance, then one
/// paced beat and the cache-space observation.
fn beat_once(engine: &mut Engine) {
    engine
        .advance_wal_publication(std::time::Instant::now(), CommitProgressMode::Reactor)
        .unwrap();
    assert!(!engine.checkpoint_beat().failed());
    engine.observe_cache_capacity();
}

/// Paced beats as the server loop runs them: a beat, the cache-space
/// observation, then a retry of the parked statement once its wait
/// generation moves. Returns the beat on which the statement completed.
fn beat_until_complete(
    engine: &mut Engine,
    session: &mut ConfiguredTransactionSession,
    sql: &str,
    beats: usize,
) -> Option<usize> {
    let mut generation = engine.cache_capacity_generation();
    for beat in 1..=beats {
        beat_once(engine);
        if engine.cache_capacity_generation() == generation {
            continue;
        }
        match run(engine, session, sql) {
            ExecutionStatus::Complete => return Some(beat),
            ExecutionStatus::Blocked {
                wait: StatementWait::CacheCapacity,
                ..
            } => generation = engine.cache_capacity_generation(),
            other => panic!("unexpected status {other:?}"),
        }
    }
    None
}

fn drain_maintenance(engine: &mut Engine, beats: usize) {
    for _ in 0..beats {
        let pending = engine
            .ckpt
            .as_ref()
            .unwrap()
            .maintenance_pending(&engine.storage)
            || engine.local_cleanup.is_some();
        if !pending {
            return;
        }
        beat_once(engine);
    }
    panic!("maintenance did not drain");
}

fn fill_heap(engine: &mut Engine) {
    let garbage = [0u8; 4096];
    while engine.storage.heap.used() * 100 < engine.storage.heap.capacity() * 85 {
        engine.storage.heap.append_bytes(&garbage).unwrap();
    }
}

#[test]
fn ddl_waiting_for_cache_space_runs_before_slow_garbage_collection() {
    let (mut engine, mut budget, config) = durable_fixture("cache-space-gc-yield", None);
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE gc_yield_rows(id integer, note text); \
         INSERT INTO gc_yield_rows SELECT i, 'row' FROM generate_series(1, 20) AS g(i)",
        false,
    );
    assert!(engine.checkpoint().unwrap());
    drain_maintenance(&mut engine, 4096);
    let orphans = engine
        .ckpt
        .as_mut()
        .unwrap()
        .put_orphan_blocks_for_test(400);
    let lsn = engine.storage.lsn();
    engine
        .ckpt
        .as_mut()
        .unwrap()
        .schedule_file_cleanup_for_test(lsn);
    // A committed change after the publication makes the next capture due.
    session.success(
        &mut engine,
        "INSERT INTO gc_yield_rows VALUES (21, 'after publication')",
        false,
    );
    fill_heap(&mut engine);
    let ddl = "CREATE TABLE gc_yield_after(k integer, a integer)";
    assert!(matches!(
        run(&mut engine, &mut session, ddl),
        ExecutionStatus::Blocked {
            wait: StatementWait::CacheCapacity,
            ..
        }
    ));
    let namespace = crate::object_store::sim::open_namespace(&config.object_store_bucket, 0);
    let completed = beat_until_complete(&mut engine, &mut session, ddl, 4096)
        .expect("DDL still waits behind garbage collection");
    let remaining = orphans
        .iter()
        .filter(|key| namespace.borrow().object_bytes(key).is_some())
        .count();
    assert!(
        remaining > orphans.len() / 2,
        "the DDL completed on beat {completed} only after collection deleted {} orphans",
        orphans.len() - remaining
    );
    assert!(!message_types(session.send.readable()).contains(&b'E'));
    // Collection restarts after the capture publishes and deletes every orphan.
    drain_maintenance(&mut engine, 4096);
    assert!(
        orphans
            .iter()
            .all(|key| namespace.borrow().object_bytes(key).is_none())
    );
    let rows = session.success(&mut engine, "SELECT count(*) FROM gc_yield_rows", false);
    assert_eq!(data_rows(&rows), ["21"]);
    drop(session);
    drop(engine);
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
}

#[test]
fn cache_space_waiter_wakes_when_maintenance_ends_without_freeing_space() {
    let (mut engine, mut budget, config) = durable_fixture("cache-space-missed-wakeup", None);
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "CREATE TABLE wake_rows(id integer)", false);
    assert!(engine.checkpoint().unwrap());
    drain_maintenance(&mut engine, 4096);
    engine.ckpt.as_mut().unwrap().put_orphan_blocks_for_test(8);
    let lsn = engine.storage.lsn();
    engine
        .ckpt
        .as_mut()
        .unwrap()
        .schedule_file_cleanup_for_test(lsn);
    // Nothing is left to capture, so the collection frees no heap space.
    fill_heap(&mut engine);
    let ddl = "CREATE TABLE wake_after(k integer)";
    assert!(matches!(
        run(&mut engine, &mut session, ddl),
        ExecutionStatus::Blocked {
            wait: StatementWait::CacheCapacity,
            ..
        }
    ));
    assert!(
        beat_until_complete(&mut engine, &mut session, ddl, 4096).is_some(),
        "the waiter slept after collection ended without freeing space"
    );
    assert!(!message_types(session.send.readable()).contains(&b'E'));
    drop(session);
    drop(engine);
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
}

#[test]
fn row_write_waits_for_checkpoint_to_free_overlay_slots() {
    let (mut engine, mut budget, config) = durable_fixture("cache-space-overlay", Some(64));
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE overlay_rows(id integer); \
         INSERT INTO overlay_rows SELECT i FROM generate_series(1, 40) AS g(i)",
        false,
    );
    engine.enable_async_block_reads();
    // The committed rows hold overlay slots until a checkpoint captures them.
    let insert = "INSERT INTO overlay_rows SELECT i FROM generate_series(41, 80) AS g(i)";
    assert!(matches!(
        run(&mut engine, &mut session, insert),
        ExecutionStatus::Blocked {
            wait: StatementWait::CacheCapacity,
            ..
        }
    ));
    assert!(engine.storage.row_capacity_waiting());
    assert!(
        beat_until_complete(&mut engine, &mut session, insert, 4096).is_some(),
        "the overlay waiter did not resume after checkpoint cleanup"
    );
    assert!(
        !message_types(session.send.readable()).contains(&b'E'),
        "{}",
        String::from_utf8_lossy(session.send.readable())
    );
    let rows = session.success(
        &mut engine,
        "SELECT count(*), sum(id) FROM overlay_rows",
        false,
    );
    assert_eq!(data_rows(&rows), ["80|3240"]);

    // A statement that needs more slots than any checkpoint can free still
    // reports the named capacity error once nothing is left to reclaim.
    drain_maintenance(&mut engine, 4096);
    let oversized = "INSERT INTO overlay_rows SELECT i FROM generate_series(1, 100) AS g(i)";
    let mut status = run(&mut engine, &mut session, oversized);
    let mut generation = engine.cache_capacity_generation();
    for _ in 0..4096 {
        if status == ExecutionStatus::Complete {
            break;
        }
        assert!(matches!(
            status,
            ExecutionStatus::Blocked {
                wait: StatementWait::CacheCapacity,
                ..
            }
        ));
        beat_once(&mut engine);
        if engine.cache_capacity_generation() != generation {
            status = run(&mut engine, &mut session, oversized);
            generation = engine.cache_capacity_generation();
        }
    }
    assert_eq!(status, ExecutionStatus::Complete);
    let output = String::from_utf8_lossy(session.send.readable()).into_owned();
    assert!(
        output.contains("map 'row_overlay_slots' is full (capacity 64)"),
        "{output}"
    );
    drop(session);
    drop(engine);
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
}

#[test]
fn row_capacity_wait_honors_statement_timeout() {
    let (mut engine, mut budget, config) = durable_fixture("cache-space-timeout", Some(64));
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE timeout_rows(id integer); \
         INSERT INTO timeout_rows SELECT i FROM generate_series(1, 40) AS g(i); \
         SET statement_timeout = '100ms'",
        false,
    );
    engine.enable_async_block_reads();
    session.send.clear();
    let insert = "INSERT INTO timeout_rows SELECT i FROM generate_series(41, 80) AS g(i)";
    assert!(matches!(
        run(&mut engine, &mut session, insert),
        ExecutionStatus::Blocked {
            wait: StatementWait::CacheCapacity,
            ..
        }
    ));
    // No checkpoint runs: the waiter's own statement deadline ends it.
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert_eq!(
        run(&mut engine, &mut session, insert),
        ExecutionStatus::Complete
    );
    let output = String::from_utf8_lossy(session.send.readable()).into_owned();
    assert!(
        output.contains("canceling statement due to statement timeout"),
        "{output}"
    );
    let rows = session.success(&mut engine, "SELECT count(*) FROM timeout_rows", false);
    assert_eq!(data_rows(&rows), ["40"]);
    drop(session);
    drop(engine);
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
}

#[test]
fn partition_routed_write_waits_for_checkpoint_to_free_overlay_slots() {
    let (mut engine, mut budget, config) = durable_fixture("cache-space-partition", Some(64));
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE routed(id integer, k integer) PARTITION BY RANGE (id); \
         CREATE TABLE routed_a PARTITION OF routed FOR VALUES FROM (0) TO (50); \
         CREATE TABLE routed_b PARTITION OF routed FOR VALUES FROM (50) TO (100); \
         CREATE INDEX ON routed (k); \
         INSERT INTO routed SELECT i, i % 7 FROM generate_series(0, 39) AS g(i)",
        false,
    );
    engine.enable_async_block_reads();
    // Rows routed to either partition wait for the checkpoint that saves the
    // committed rows, as a plain table's write does.
    let insert = "INSERT INTO routed SELECT i, i % 7 FROM generate_series(40, 79) AS g(i)";
    assert!(matches!(
        run(&mut engine, &mut session, insert),
        ExecutionStatus::Blocked {
            wait: StatementWait::CacheCapacity,
            ..
        }
    ));
    assert!(
        beat_until_complete(&mut engine, &mut session, insert, 4096).is_some(),
        "the routed waiter did not resume after checkpoint cleanup"
    );
    assert!(
        !message_types(session.send.readable()).contains(&b'E'),
        "{}",
        String::from_utf8_lossy(session.send.readable())
    );
    let rows = session.success(
        &mut engine,
        "SELECT count(*), sum(id) FROM routed WHERE k = 3",
        false,
    );
    assert_eq!(data_rows(&rows), ["11|418"]);
    drop(session);
    drop(engine);
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
}
