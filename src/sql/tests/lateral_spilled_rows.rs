//! A `LATERAL` subquery the join search plans reads its table's rows
//! wherever they live: in the memtable, in spilled generations after a
//! checkpoint, and in object storage after a restart.

use super::*;

const COUNTED: &str = "SELECT count(*), sum(s.count) FROM lo, \
     LATERAL (SELECT count(*) FROM li WHERE li.b = lo.a) s WHERE lo.x <= 30";
const LISTED: &str = "SELECT * FROM lo, \
     LATERAL (SELECT c FROM li WHERE li.b = lo.a ORDER BY c LIMIT 2) s \
     WHERE lo.x BETWEEN 5 AND 6";

fn check(engine: &mut Engine, budget: &mut Budget, when: &str) {
    assert_eq!(
        data_rows(&run_with(engine, budget, COUNTED)),
        ["30|1500"],
        "{when}"
    );
    assert_eq!(
        data_rows(&run_with(engine, budget, LISTED)),
        ["5|5|5", "5|5|105", "6|6|6", "6|6|106"],
        "{when}"
    );
}

#[test]
fn lateral_subqueries_read_spilled_rows() {
    use core::sync::atomic::{AtomicU32, Ordering};

    static NEXT_NAMESPACE: AtomicU32 = AtomicU32::new(0);
    let sequence = NEXT_NAMESPACE.fetch_add(1, Ordering::SeqCst);
    let mut config = test_config(&format!("lateral-spilled-rows-{sequence}"));
    config.object_store_on = true;
    config.object_store_sim = true;
    config.object_store_bucket = format!("lateral-spilled-rows-{}-{sequence}", std::process::id());
    config.object_store_response_bytes = 1 << 20;
    config.memtable_bytes = 256 * 1024;
    config.row_overlay_slots = 16384;
    config.wal_buffer_bytes = 256 * 1024;
    config.wal_upload_buffer_bytes = 256 * 1024;
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);

    let mut budget = test_engine_budget(&config, (1 << 29) + (96 << 20));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    for sql in [
        // A dropped table's spilled rows first, as fixture order leaves
        // them in the differential suite.
        "CREATE TYPE c962 AS (x int, y text)",
        "CREATE TYPE m962 AS (x int, m money)",
        "CREATE TABLE t962 (a int, b text, k c962, km m962, ka c962[])",
        "INSERT INTO t962 SELECT i % 40, 'b' || (i % 9), ROW(i % 30, 'y' || (i % 4))::c962, \
         ROW(i % 6, (i % 5)::money)::m962, ARRAY[ROW(i % 20, 'z')::c962] \
         FROM generate_series(1, 750) i",
        "INSERT INTO t962 SELECT i % 40, 'b' || (i % 9), ROW(i % 30, 'y' || (i % 4))::c962, \
         ROW(i % 6, (i % 5)::money)::m962, ARRAY[ROW(i % 20, 'z')::c962] \
         FROM generate_series(751, 1500) i",
        "DROP TABLE t962",
        "DROP TYPE c962, m962",
        "CREATE TABLE lo (a int, x int)",
        "INSERT INTO lo SELECT g % 10, g FROM generate_series(1, 1000) g",
        "CREATE TABLE li (b int, c int, d text)",
        // In statements of up to 1,000 rows, a test transaction's limit.
        "INSERT INTO li SELECT g % 100, g, 'd' || g FROM generate_series(1, 1000) g",
        "INSERT INTO li SELECT g % 100, g, 'd' || g FROM generate_series(1001, 2000) g",
        "INSERT INTO li SELECT g % 100, g, 'd' || g FROM generate_series(2001, 3000) g",
        "INSERT INTO li SELECT g % 100, g, 'd' || g FROM generate_series(3001, 4000) g",
        "INSERT INTO li SELECT g % 100, g, 'd' || g FROM generate_series(4001, 5000) g",
        "ANALYZE lo, li",
    ] {
        let output = run_with(&mut engine, &mut budget, sql);
        assert!(
            !String::from_utf8_lossy(&output).contains("ERROR"),
            "{sql}: {}",
            String::from_utf8_lossy(&output)
        );
    }
    check(&mut engine, &mut budget, "in the memtable");
    assert!(engine.checkpoint().unwrap());
    check(&mut engine, &mut budget, "after a checkpoint");
    drop(engine);

    let mut budget = test_engine_budget(&config, (1 << 29) + (96 << 20));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    super::object_reads::replace_uncached_blocks(&mut engine, &config);
    check(
        &mut engine,
        &mut budget,
        "after a restart, read from objects",
    );
    drop(engine);
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    std::fs::remove_dir_all(config.data_dir).unwrap();
}
