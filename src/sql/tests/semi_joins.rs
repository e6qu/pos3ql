use super::*;

/// The rows of `query` from the general executor and from a cursor, which
/// must agree.
fn both_executors(
    session: &mut ConfiguredTransactionSession,
    engine: &mut Engine,
    query: &str,
) -> Vec<String> {
    let general = data_rows(&session.success(engine, query, true));
    let cursor = data_rows(&session.success(
        engine,
        &format!("BEGIN; DECLARE sj CURSOR FOR {query}; FETCH ALL FROM sj; CLOSE sj; COMMIT"),
        true,
    ));
    assert_eq!(general, cursor, "{query}");
    general
}

/// `EXISTS`, `NOT EXISTS` and `IN` subqueries of `WHERE` become semi and
/// anti joins, as PostgreSQL's `pull_up_sublinks` makes them.
#[test]
fn where_subqueries_become_semi_and_anti_joins() {
    let config = test_config("semi-anti-joins");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE t (id int, x int, y text); CREATE TABLE u (id int, tid int, z text); \
         CREATE TABLE w (k int); \
         INSERT INTO t VALUES (1, 10, 'a'), (2, 20, 'b'), (3, NULL, 'c'), (NULL, 40, 'd'), (5, 50, NULL); \
         INSERT INTO u VALUES (1, 1, 'p'), (2, 1, 'q'), (3, 3, 'r'), (4, NULL, 's'), (5, 9, NULL); \
         INSERT INTO w VALUES (1), (3), (NULL)",
        true,
    );
    // PostgreSQL 18.6 returns these rows.
    let cases: &[(&str, &[&str])] = &[
        (
            "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.tid = t.id) ORDER BY id",
            &["1|10|a", "3|NULL|c"],
        ),
        (
            "SELECT * FROM t WHERE NOT EXISTS (SELECT 1 FROM u WHERE u.tid = t.id) ORDER BY id",
            &["2|20|b", "5|50|NULL", "NULL|40|d"],
        ),
        (
            "SELECT * FROM t WHERE id IN (SELECT tid FROM u) ORDER BY id",
            &["1|10|a", "3|NULL|c"],
        ),
        (
            "SELECT * FROM t WHERE id IN (SELECT k FROM w) \
             AND EXISTS (SELECT 1 FROM u WHERE tid = t.id AND z > 'p') ORDER BY id",
            &["1|10|a", "3|NULL|c"],
        ),
        (
            "SELECT t.id FROM t LEFT JOIN u ON t.id = u.id \
             WHERE NOT EXISTS (SELECT 1 FROM w WHERE w.k = u.tid) ORDER BY 1",
            &["5", "NULL"],
        ),
        (
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.tid = t.id OR u.z = t.y) \
             ORDER BY id",
            &["1", "3"],
        ),
    ];
    for (query, expected) in cases {
        assert_eq!(
            both_executors(&mut session, &mut engine, query),
            *expected,
            "{query}"
        );
    }
    // The subquery's relation stays out of reach of the query's own names.
    let output = session.execute(
        &mut engine,
        "SELECT tid FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.tid = t.id)",
        true,
    );
    assert_eq!(error_sqlstates(&output), ["42703"]);

    session.success(
        &mut engine,
        "CREATE TABLE big (id int, v int); CREATE TABLE small (id int, aid int, w int); \
         INSERT INTO big SELECT g, g % 7 FROM generate_series(1, 1000) g; \
         INSERT INTO small SELECT g, g * 3, g FROM generate_series(1, 20) g; \
         ANALYZE big; ANALYZE small",
        true,
    );
    // PostgreSQL hashes the smaller side and loops over a materialized
    // copy when no equality joins the two.
    let plans: &[(&str, &[&str])] = &[
        (
            "SELECT * FROM big WHERE EXISTS (SELECT 1 FROM small WHERE small.aid = big.id)",
            &[
                "Hash Semi Join",
                "  Hash Cond: (big.id = small.aid)",
                "  ->  Seq Scan on big",
                "  ->  Hash",
                "        ->  Seq Scan on small",
            ],
        ),
        (
            "SELECT * FROM small WHERE NOT EXISTS (SELECT 1 FROM big WHERE big.id = small.aid)",
            &[
                "Hash Right Anti Join",
                "  Hash Cond: (big.id = small.aid)",
                "  ->  Seq Scan on big",
                "  ->  Hash",
                "        ->  Seq Scan on small",
            ],
        ),
        (
            "SELECT * FROM big WHERE v > 3 AND id IN (SELECT aid FROM small WHERE w < 10)",
            &[
                "Hash Semi Join",
                "  Hash Cond: (big.id = small.aid)",
                "  ->  Seq Scan on big",
                "        Filter: (v > 3)",
                "  ->  Hash",
                "        ->  Seq Scan on small",
                "              Filter: (w < 10)",
            ],
        ),
        (
            "SELECT * FROM big WHERE NOT EXISTS (SELECT 1 FROM small WHERE small.w > big.v)",
            &[
                "Nested Loop Anti Join",
                "  Join Filter: (small.w > big.v)",
                "  ->  Seq Scan on big",
                "  ->  Materialize",
                "        ->  Seq Scan on small",
            ],
        ),
    ];
    for (query, expected) in plans {
        let rows =
            data_rows(&session.success(&mut engine, &format!("EXPLAIN (COSTS OFF) {query}"), true));
        assert_eq!(rows, *expected, "{query}");
    }
    // The materialized copy reads its scan only as far as a loop needed.
    let rows = data_rows(&session.success(
        &mut engine,
        "EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) \
         SELECT * FROM big WHERE NOT EXISTS (SELECT 1 FROM small WHERE small.w > big.v)",
        true,
    ));
    assert!(
        rows.iter()
            .any(|row| row == "        ->  Seq Scan on small (actual rows=7.00 loops=1)"),
        "{rows:?}"
    );
}
