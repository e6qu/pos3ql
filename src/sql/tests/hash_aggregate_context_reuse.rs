//! PostgreSQL's aset.c reuses freed memory contexts of the default sizes
//! from a per-backend freelist without recomputing their chunk limit, so a
//! hash aggregate after one built under `work_mem` below 1MB may count its
//! bucket arrays differently. pos3ql does not track which context the
//! freelist returns: such a later aggregate fails with `0A000`, and a new
//! session starts clean.

use super::compatibility_diagnostics::with_engine;
use super::*;

/// The rows after the statistics go in by 1000, under the test
/// transaction's row limit.
const SETUP: [&str; 9] = [
    "CREATE TABLE reuse (a int) WITH (autovacuum_enabled = off)",
    "INSERT INTO reuse SELECT i % 20 FROM generate_series(1, 200) i",
    "ANALYZE reuse",
    "INSERT INTO reuse SELECT (i * 7919) % 5000 FROM generate_series(201, 1200) i",
    "INSERT INTO reuse SELECT (i * 7919) % 5000 FROM generate_series(1201, 2200) i",
    "INSERT INTO reuse SELECT (i * 7919) % 5000 FROM generate_series(2201, 3200) i",
    "INSERT INTO reuse SELECT (i * 7919) % 5000 FROM generate_series(3201, 4200) i",
    "INSERT INTO reuse SELECT (i * 7919) % 5000 FROM generate_series(4201, 5200) i",
    "INSERT INTO reuse SELECT (i * 7919) % 5000 FROM generate_series(5201, 6200) i",
];

const REUSE: &str = "the memory of a hash aggregate after an earlier hash aggregate of this session ran with work_mem below 1MB is not supported: PostgreSQL reuses that aggregate's memory context, which pos3ql does not model; start a new session, or run every hash aggregate of the session with work_mem of at least 1MB";

fn session(engine: &mut Engine, budget: &mut Budget, guc: &GucState, sql: &str) -> Vec<u8> {
    run_with_guc(engine, budget, sql, 32 << 20, guc)
}

#[test]
fn work_contexts_take_the_chunk_limit_of_their_largest_block() {
    for (work_mem, limit) in [
        (64, 1024),
        (128, 1024),
        (256, 2048),
        (512, 4096),
        (1024, 8192),
        (4096, 8192),
    ] {
        assert_eq!(
            guc::work_context_chunk_limit(work_mem),
            limit,
            "{work_mem}kB"
        );
    }
}

#[test]
fn a_hash_aggregate_after_a_small_work_mem_one_fails_until_a_new_session() {
    with_engine("hash-aggregate-context-reuse", |engine, budget| {
        for sql in SETUP {
            assert!(
                error_sqlstates(&run_with(engine, budget, sql)).is_empty(),
                "{sql}"
            );
        }
        let query = "SELECT DISTINCT a FROM reuse";
        for (work_mem, earlier) in [
            ("64kB", "EXPLAIN (COSTS OFF) SELECT DISTINCT a FROM reuse"),
            ("512kB", "EXPLAIN (COSTS OFF) SELECT DISTINCT a FROM reuse"),
            ("64kB", "SELECT DISTINCT a FROM reuse LIMIT 1"),
        ] {
            let guc = GucState::new();
            session(
                engine,
                budget,
                &guc,
                &format!("SET work_mem = '{work_mem}'"),
            );
            session(engine, budget, &guc, earlier);
            session(engine, budget, &guc, "SET work_mem = '64kB'");
            session(engine, budget, &guc, "SET hash_mem_multiplier = 1");
            let output = session(engine, budget, &guc, query);
            assert_eq!(
                error_sqlstates(&output),
                ["0A000"],
                "after {earlier} at {work_mem}"
            );
            assert!(String::from_utf8_lossy(&output).contains(REUSE));
            // A new session's freelist holds no such context.
            let fresh = GucState::new();
            session(engine, budget, &fresh, "SET work_mem = '64kB'");
            session(engine, budget, &fresh, "SET hash_mem_multiplier = 1");
            let output = session(engine, budget, &fresh, query);
            assert!(error_sqlstates(&output).is_empty(), "a new session");
            assert_eq!(data_rows(&output).len(), 5000);
        }
        // Hash aggregates under 1MB or more return contexts of the default
        // chunk limit.
        let guc = GucState::new();
        session(engine, budget, &guc, "SET work_mem = '1MB'");
        session(engine, budget, &guc, "SELECT DISTINCT a FROM reuse LIMIT 1");
        session(engine, budget, &guc, "SET work_mem = '64kB'");
        session(engine, budget, &guc, "SET hash_mem_multiplier = 1");
        let output = session(engine, budget, &guc, query);
        assert!(error_sqlstates(&output).is_empty());
    });
}
