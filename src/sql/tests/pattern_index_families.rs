//! A btree pattern operator class indexes `=` and the byte-wise `~<~`
//! family; a default class indexes the collation-aware comparisons. Neither
//! serves the other's ordering, as in PostgreSQL 18.6.

use super::compatibility_diagnostics::with_engine;
use super::*;

#[test]
fn btree_index_constraints_follow_their_operator_family() {
    with_engine("pattern-index-families", |engine, budget| {
        for sql in [
            "CREATE TABLE words (id int, w text)",
            "INSERT INTO words SELECT g, 'w' || g FROM generate_series(1, 1000) g",
        ] {
            let out = run_with(engine, budget, sql);
            assert!(
                error_sqlstates(&out).is_empty(),
                "{sql}: {}",
                String::from_utf8_lossy(&out)
            );
        }
        for sql in [
            "CREATE INDEX words_w ON words (w text_pattern_ops)",
            "ANALYZE words",
        ] {
            run_with(engine, budget, sql);
        }
        let plan = |engine: &mut Engine, budget: &mut Budget, query: &str| {
            data_rows(&run_with(
                engine,
                budget,
                &format!("EXPLAIN (COSTS OFF) {query}"),
            ))
        };
        assert_eq!(
            plan(engine, budget, "SELECT w FROM words WHERE w < 'w100'"),
            ["Seq Scan on words", "  Filter: (w < 'w100'::text)"]
        );
        assert_eq!(
            data_rows(&run_with(
                engine,
                budget,
                "SELECT w FROM words WHERE w ~>=~ 'w998' AND w ~<~ 'w9990' ORDER BY w"
            )),
            ["w998", "w999"]
        );
        run_with(engine, budget, "DROP INDEX words_w");
        run_with(engine, budget, "CREATE INDEX words_d ON words (w)");
        assert_eq!(
            plan(engine, budget, "SELECT w FROM words WHERE w ~>=~ 'w999'"),
            ["Seq Scan on words", "  Filter: (w ~>=~ 'w999'::text)"]
        );
    });
}
