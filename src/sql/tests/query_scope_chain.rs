//! Name resolution across enclosing queries: nested subqueries and derived
//! tables read every enclosing query, and a derived table reads the FROM
//! items before it only when `LATERAL`, as in PostgreSQL 18.6.

use super::compatibility_diagnostics::{diagnostics, with_engine};
use super::*;

fn setup(engine: &mut Engine, budget: &mut Budget) {
    for sql in [
        "CREATE TABLE a (x int)",
        "CREATE TABLE b (y int)",
        "CREATE TABLE c (z int)",
        "INSERT INTO a VALUES (1), (2), (3)",
        "INSERT INTO b VALUES (1), (2)",
        "INSERT INTO c VALUES (1), (3)",
    ] {
        run_with(engine, budget, sql);
    }
}

#[test]
fn nested_subqueries_read_every_enclosing_query() {
    with_engine("scope-chain-nested", |engine, budget| {
        setup(engine, budget);
        for (sql, rows) in [
            (
                "SELECT (SELECT (SELECT a.x * 10 + b.y FROM c LIMIT 1) FROM b ORDER BY y LIMIT 1) \
                 FROM a ORDER BY 1",
                &["11", "21", "31"][..],
            ),
            (
                "SELECT x, (SELECT count(*) FROM b WHERE EXISTS \
                 (SELECT 1 FROM c WHERE c.z = b.y AND a.x = 1)) FROM a ORDER BY 1",
                &["1|1", "2|0", "3|0"][..],
            ),
            (
                "SELECT x, (SELECT (SELECT b.y + a.x FROM c LIMIT 1) FROM b GROUP BY b.y \
                 ORDER BY 1 LIMIT 1) FROM a ORDER BY 1",
                &["1|2", "2|3", "3|4"][..],
            ),
            (
                "SELECT x, (SELECT max((SELECT b.y + a.x FROM c LIMIT 1)) FROM b) FROM a ORDER BY 1",
                &["1|3", "2|4", "3|5"][..],
            ),
            (
                "SELECT x, (SELECT y FROM b WHERE y = a.x UNION SELECT z FROM c WHERE z = a.x \
                 ORDER BY 1 LIMIT 1) FROM a ORDER BY 1",
                &["1|1", "2|2", "3|3"][..],
            ),
            (
                "SELECT x, (SELECT y FROM b WHERE y = a.x GROUP BY y HAVING a.x > 1) \
                 FROM a ORDER BY 1",
                &["1|NULL", "2|2", "3|NULL"][..],
            ),
            (
                "SELECT x, (SELECT min(t.s) FROM (SELECT y + a.x AS s FROM b) t) FROM a ORDER BY 1",
                &["1|2", "2|3", "3|4"][..],
            ),
            (
                "SELECT x, (WITH q AS (SELECT a.x AS v) SELECT v FROM q) FROM a ORDER BY 1",
                &["1|1", "2|2", "3|3"][..],
            ),
            (
                "SELECT * FROM a JOIN (b JOIN LATERAL (SELECT a.x + b.y AS z) s ON true) ON true \
                 WHERE x = 1 ORDER BY 2",
                &["1|1|2", "1|2|3"][..],
            ),
        ] {
            let output = run_with(engine, budget, sql);
            assert_eq!(data_rows(&output), rows, "{sql}");
        }
    });
}

#[test]
fn derived_tables_reach_earlier_items_only_when_lateral() {
    with_engine("scope-chain-derived", |engine, budget| {
        setup(engine, budget);
        let lateral_table = "hint: To reference that table, you must mark this subquery with LATERAL. \
             detail: There is an entry for table \"a\", but it cannot be referenced from this part of the query.";
        for (sql, expected) in [
            (
                "SELECT * FROM a, (SELECT * FROM b WHERE b.y = a.x) s",
                format!(
                    "ERROR 42P01 invalid reference to FROM-clause entry for table \"a\" @47 {lateral_table}"
                ),
            ),
            (
                "SELECT * FROM a, (SELECT * FROM b WHERE y = x) s",
                "ERROR 42703 column \"x\" does not exist @45 hint: To reference that column, \
                 you must mark this subquery with LATERAL. detail: There is a column named \"x\" \
                 in table \"a\", but it cannot be referenced from this part of the query."
                    .to_owned(),
            ),
            (
                "SELECT * FROM a, (SELECT y FROM b) s(x), (SELECT x) q",
                "ERROR 42703 column \"x\" does not exist @50 hint: Try using a table-qualified \
                 name. detail: There are columns named \"x\", but they are in tables that cannot \
                 be referenced from this part of the query."
                    .to_owned(),
            ),
            (
                "UPDATE a SET x = s.v FROM (SELECT a.x AS v) s",
                "ERROR 42P01 invalid reference to FROM-clause entry for table \"a\" @35 detail: \
                 There is an entry for table \"a\", but it cannot be referenced from this part of \
                 the query."
                    .to_owned(),
            ),
            (
                "UPDATE a SET x = s.v FROM LATERAL (SELECT a.x AS v) s",
                "ERROR 42P10 invalid reference to FROM-clause entry for table \"a\" @43 hint: \
                 There is an entry for table \"a\", but it cannot be referenced from this part of \
                 the query."
                    .to_owned(),
            ),
            (
                "SELECT * FROM a RIGHT JOIN LATERAL (SELECT a.x) s ON true",
                "ERROR 42P10 invalid reference to FROM-clause entry for table \"a\" @44 detail: \
                 The combining JOIN type must be INNER or LEFT for a LATERAL reference."
                    .to_owned(),
            ),
        ] {
            let output = run_with(engine, budget, sql);
            assert_eq!(diagnostics(&output), [expected], "{sql}");
        }
        let output = run_with(
            engine,
            budget,
            "SELECT x, (SELECT count(*) FROM b, (SELECT * FROM c WHERE c.z = a.x) s) \
             FROM a ORDER BY 1",
        );
        assert_eq!(data_rows(&output), ["1|2", "2|0", "3|2"]);
    });
}
