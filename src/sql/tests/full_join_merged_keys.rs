//! A full join's merged `USING` column as a grouping key and equated with
//! a constant, and `USING` joins beside parenthesized joins, planned by the
//! join search as PostgreSQL 18.6 plans them.

use super::compatibility_diagnostics::{diagnostics, with_engine};
use super::*;

#[test]
fn full_join_merged_columns_group_and_push_constants() {
    with_engine("full-join-merged-keys", |engine, budget| {
        for sql in [
            "CREATE TABLE fu (id int, v int)",
            "INSERT INTO fu SELECT g, g % 7 FROM generate_series(1, 200) g",
            "ANALYZE fu",
            "CREATE TABLE pa (id int, v int)",
            "INSERT INTO pa SELECT g, g % 3 FROM generate_series(1, 100) g",
            "CREATE TABLE pb (id int, w int)",
            "INSERT INTO pb SELECT g, g % 5 FROM generate_series(1, 50) g",
            "CREATE TABLE pc (id int, x int)",
            "INSERT INTO pc SELECT g % 100, g FROM generate_series(1, 300) g",
            "CREATE TABLE pd (id int, y int)",
            "INSERT INTO pd SELECT g, g FROM generate_series(1, 20) g",
            "ANALYZE pa, pb, pc, pd",
        ] {
            run_with(engine, budget, sql);
        }
        let plan = |engine: &mut Engine, budget: &mut Budget, sql: &str| {
            data_rows(&run_with(
                engine,
                budget,
                &format!("EXPLAIN (COSTS OFF) {sql}"),
            ))
        };
        assert_eq!(
            plan(
                engine,
                budget,
                "SELECT id, count(*) FROM fu t0 FULL JOIN fu t1 USING (id) \
                 FULL JOIN fu t2 USING (id) GROUP BY id"
            ),
            [
                "HashAggregate",
                "  Group Key: COALESCE(COALESCE(t0.id, t1.id), t2.id)",
                "  ->  Hash Full Join",
                "        Hash Cond: (COALESCE(t0.id, t1.id) = t2.id)",
                "        ->  Hash Full Join",
                "              Hash Cond: (t0.id = t1.id)",
                "              ->  Seq Scan on fu t0",
                "              ->  Hash",
                "                    ->  Seq Scan on fu t1",
                "        ->  Hash",
                "              ->  Seq Scan on fu t2",
            ]
        );
        // The constant reaches every relation through the lower join's
        // `COALESCE`, and neither full join keeps a condition.
        assert_eq!(
            plan(
                engine,
                budget,
                "SELECT * FROM fu t0 FULL JOIN fu t1 USING (id) FULL JOIN fu t2 USING (id) \
                 WHERE id = 5"
            ),
            [
                "Merge Full Join",
                "  ->  Merge Full Join",
                "        ->  Seq Scan on fu t0",
                "              Filter: (id = 5)",
                "        ->  Materialize",
                "              ->  Seq Scan on fu t1",
                "                    Filter: (id = 5)",
                "  ->  Materialize",
                "        ->  Seq Scan on fu t2",
                "              Filter: (id = 5)",
            ]
        );
        assert_eq!(
            data_rows(&run_with(
                engine,
                budget,
                "SELECT * FROM fu t0 FULL JOIN fu t1 USING (id) FULL JOIN fu t2 USING (id) \
                 WHERE id = 5"
            )),
            ["5|5|5|5"]
        );
        // A left join's clause of the merged column moves the constant to
        // its inner side.
        assert_eq!(
            plan(
                engine,
                budget,
                "SELECT * FROM fu t0 FULL JOIN fu t1 USING (id) LEFT JOIN fu t2 USING (id) \
                 WHERE id = 9"
            ),
            [
                "Nested Loop Left Join",
                "  ->  Merge Full Join",
                "        ->  Seq Scan on fu t0",
                "              Filter: (id = 9)",
                "        ->  Materialize",
                "              ->  Seq Scan on fu t1",
                "                    Filter: (id = 9)",
                "  ->  Seq Scan on fu t2",
                "        Filter: (id = 9)",
            ]
        );
        // A sorted grouping reads the key from its input, parenthesized.
        assert_eq!(
            data_rows(&run_with(
                engine,
                budget,
                "SET enable_hashagg = off; EXPLAIN (COSTS OFF) SELECT id, count(*) \
                 FROM fu t0 FULL JOIN fu t1 USING (id) GROUP BY id; \
                 SELECT id, count(*) FROM fu t0 FULL JOIN fu t1 USING (id) \
                 GROUP BY id ORDER BY id DESC LIMIT 2; RESET enable_hashagg"
            ))[..2],
            ["GroupAggregate", "  Group Key: (COALESCE(t0.id, t1.id))"]
        );
        // A `USING` join beside a parenthesized join reads its members.
        assert_eq!(
            data_rows(&run_with(
                engine,
                budget,
                "SELECT count(*), sum(x) FROM pa JOIN (pb JOIN pc USING (id)) USING (id)"
            )),
            ["150|18825"]
        );
        // One nested in a parenthesized join inside another is refused
        // rather than run in pos3ql's own join order.
        assert_eq!(
            diagnostics(&run_with(
                engine,
                budget,
                "SELECT * FROM pa JOIN (pb JOIN (pc JOIN pd USING (id)) USING (id)) USING (id)"
            )),
            [
                "ERROR 0A000 a parenthesized join's USING or NATURAL join in this query is not \
              supported by the join search"
            ]
        );
    });
}
