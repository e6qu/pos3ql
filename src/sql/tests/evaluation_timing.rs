//! When a materialized `WITH` query and an `IN`, `ANY` or `ALL` subquery with
//! effects run, compared with PostgreSQL 18.6: where evaluation first reaches
//! them, once per statement unless correlated.

use super::compatibility_diagnostics::with_engine;
use super::*;

fn calls(engine: &mut Engine, budget: &mut Budget, sql: &str) -> Vec<String> {
    run_with(engine, budget, "TRUNCATE calls");
    run_with(engine, budget, sql);
    data_rows(&run_with(
        engine,
        budget,
        "SELECT what FROM calls ORDER BY n",
    ))
}

#[test]
fn with_queries_and_quantified_subqueries_run_where_first_reached() {
    with_engine("evaluation-timing", |engine, budget| {
        for sql in [
            "CREATE SEQUENCE call_order",
            "CREATE TABLE calls (n int, what text)",
            "CREATE FUNCTION note(t text) RETURNS int LANGUAGE plpgsql VOLATILE AS \
             $$ BEGIN INSERT INTO calls VALUES (nextval('call_order'), t); RETURN 1; END $$",
        ] {
            run_with(engine, budget, sql);
        }
        assert_eq!(
            calls(
                engine,
                budget,
                "WITH c AS MATERIALIZED (SELECT note('cte') a) \
                 SELECT note('main'), (SELECT a FROM c)",
            ),
            ["main", "cte"]
        );
        // A WITH query nothing reads never runs, nor fails.
        assert!(
            calls(
                engine,
                budget,
                "WITH c AS MATERIALIZED (SELECT note('cte') a) \
                 SELECT 1 FROM generate_series(1, 0) g WHERE g IN (SELECT a FROM c)",
            )
            .is_empty()
        );
        assert_eq!(
            data_rows(&run_with(
                engine,
                budget,
                "WITH c AS MATERIALIZED (SELECT 1 / 0 a) SELECT 1 WHERE false AND 1 IN (SELECT a FROM c)",
            )),
            Vec::<String>::new()
        );
        assert!(
            calls(
                engine,
                budget,
                "SELECT CASE WHEN false THEN 1 IN (SELECT note('in')) ELSE true END",
            )
            .is_empty()
        );
        assert_eq!(
            calls(
                engine,
                budget,
                "SELECT note('first') = 1 AND 1 IN (SELECT note('second'))",
            ),
            ["first", "second"]
        );
        // An uncorrelated list runs once; a correlated one once per row.
        assert_eq!(
            calls(
                engine,
                budget,
                "SELECT x = ANY (SELECT note('any') + g FROM generate_series(0, 2) g) \
                 FROM generate_series(1, 3) x",
            ),
            ["any", "any", "any"]
        );
        assert_eq!(
            calls(
                engine,
                budget,
                "SELECT x IN (SELECT note('in' || x)) FROM generate_series(1, 3) x",
            ),
            ["in1", "in2", "in3"]
        );
    });
}
