//! An index-only scan fetches the heap tuple of every row whose page the
//! visibility map does not mark all-visible. Only VACUUM sets that map, so
//! where a row lives (memtable, spilled generations after a checkpoint, or
//! objects after a restart) never changes the count.

use super::*;

const PLAN: &str = "EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) \
     SELECT * FROM hf WHERE id = (SELECT max(id) FROM hf)";

fn heap_fetches(engine: &mut Engine, budget: &mut Budget, when: &str) -> Vec<String> {
    let plan = data_rows(&run_with(engine, budget, PLAN));
    let fetches: Vec<String> = plan
        .iter()
        .filter(|line| line.contains("Heap Fetches"))
        .map(|line| line.trim().to_owned())
        .collect();
    assert_eq!(fetches.len(), 1, "{when}: {plan:?}");
    fetches
}

#[test]
fn index_only_scans_fetch_unvacuumed_spilled_rows() {
    use core::sync::atomic::{AtomicU32, Ordering};

    static NEXT_NAMESPACE: AtomicU32 = AtomicU32::new(0);
    let sequence = NEXT_NAMESPACE.fetch_add(1, Ordering::SeqCst);
    let mut config = test_config(&format!("index-only-spilled-{sequence}"));
    config.object_store_on = true;
    config.object_store_sim = true;
    config.object_store_bucket = format!("index-only-spilled-{}-{sequence}", std::process::id());
    config.object_store_response_bytes = 1 << 20;
    config.memtable_bytes = 256 * 1024;
    config.row_overlay_slots = 16384;
    config.wal_buffer_bytes = 256 * 1024;
    config.wal_upload_buffer_bytes = 256 * 1024;
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);

    let mut budget = test_engine_budget(&config, (1 << 29) + (96 << 20));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    for sql in [
        // A vacuumed, dropped table first: its slot's visibility map must
        // not carry over.
        "CREATE TABLE gone (id int PRIMARY KEY, v text)",
        "INSERT INTO gone SELECT i, 'g' || i FROM generate_series(1, 1000) i",
        "VACUUM gone",
        "DROP TABLE gone",
        "CREATE TABLE hf (id int PRIMARY KEY, k int, v text)",
        "INSERT INTO hf SELECT i, i % 100, 'v' || i FROM generate_series(1, 1000) i",
        "INSERT INTO hf SELECT i, i % 100, 'v' || i FROM generate_series(1001, 2000) i",
        "INSERT INTO hf SELECT i, i % 100, 'v' || i FROM generate_series(2001, 3000) i",
        "ANALYZE hf",
    ] {
        let output = run_with(&mut engine, &mut budget, sql);
        assert!(
            !String::from_utf8_lossy(&output).contains("ERROR"),
            "{sql}: {}",
            String::from_utf8_lossy(&output)
        );
    }
    let expected = ["Heap Fetches: 1"];
    assert_eq!(
        heap_fetches(&mut engine, &mut budget, "in the memtable"),
        expected
    );
    assert!(engine.checkpoint().unwrap());
    assert_eq!(
        heap_fetches(&mut engine, &mut budget, "after a checkpoint"),
        expected
    );
    drop(engine);

    let mut budget = test_engine_budget(&config, (1 << 29) + (96 << 20));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    super::object_reads::replace_uncached_blocks(&mut engine, &config);
    assert_eq!(
        heap_fetches(&mut engine, &mut budget, "after a restart"),
        expected
    );
    let output = run_with(&mut engine, &mut budget, "VACUUM hf");
    assert!(!String::from_utf8_lossy(&output).contains("ERROR"));
    assert_eq!(
        heap_fetches(&mut engine, &mut budget, "after VACUUM"),
        ["Heap Fetches: 0"]
    );
    drop(engine);
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    std::fs::remove_dir_all(config.data_dir).unwrap();
}
