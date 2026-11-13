use super::*;

/// The rows of `query` from the general executor and from a cursor's
/// retained reader, which must agree.
fn both_executors(
    session: &mut ConfiguredTransactionSession,
    engine: &mut Engine,
    query: &str,
) -> Vec<String> {
    let general = data_rows(&session.success(engine, query, true));
    let cursor = data_rows(&session.success(
        engine,
        &format!("BEGIN; DECLARE hoj CURSOR FOR {query}; FETCH ALL FROM hoj; CLOSE hoj; COMMIT"),
        true,
    ));
    assert_eq!(general, cursor, "{query}");
    general
}

fn plan(
    session: &mut ConfiguredTransactionSession,
    engine: &mut Engine,
    query: &str,
) -> Vec<String> {
    data_rows(&session.success(engine, &format!("EXPLAIN (COSTS OFF) {query}"), true))
}

#[test]
fn right_and_full_joins_hash_with_build_match_flags() {
    let config = test_config("hash-outer-joins");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE ha(k int, k2 text, v int); CREATE TABLE hb(k int, k2 text, w int); \
         CREATE TABLE he(k int, w int); \
         INSERT INTO ha VALUES (1,'x',10),(1,'x',11),(2,'y',20),(3,'z',30),(NULL,'n',40),(5,NULL,50); \
         INSERT INTO hb VALUES (1,'x',100),(1,'x',101),(2,'q',200),(4,'z',400),(NULL,'n',500),(5,NULL,600)",
        true,
    );
    // PostgreSQL 18.6 returns these rows.
    let cases: &[(&str, &[&str])] = &[
        (
            "SELECT a.v, b.w FROM ha a RIGHT JOIN hb b ON a.k = b.k ORDER BY b.w, a.v",
            &[
                "10|100", "11|100", "10|101", "11|101", "20|200", "NULL|400", "NULL|500", "50|600",
            ],
        ),
        (
            "SELECT a.v, b.w FROM ha a FULL JOIN hb b ON a.k = b.k ORDER BY a.v NULLS FIRST, b.w",
            &[
                "NULL|400", "NULL|500", "10|100", "10|101", "11|100", "11|101", "20|200",
                "30|NULL", "40|NULL", "50|600",
            ],
        ),
        // A hashed row counts as matched only when the whole condition holds.
        (
            "SELECT a.v, b.w FROM ha a RIGHT JOIN hb b ON a.k = b.k AND a.v + 90 < b.w \
             ORDER BY b.w, a.v",
            &[
                "NULL|100", "10|101", "20|200", "NULL|400", "NULL|500", "50|600",
            ],
        ),
        (
            "SELECT a.v, b.w FROM ha a FULL JOIN hb b ON a.k = b.k AND a.v + 90 < b.w \
             ORDER BY a.v NULLS FIRST, b.w",
            &[
                "NULL|100", "NULL|400", "NULL|500", "10|101", "11|NULL", "20|200", "30|NULL",
                "40|NULL", "50|600",
            ],
        ),
        // Multi-column keys; a NULL in either key column never matches.
        (
            "SELECT a.v, b.w FROM ha a FULL JOIN hb b ON a.k = b.k AND a.k2 = b.k2 \
             ORDER BY a.v NULLS FIRST, b.w",
            &[
                "NULL|200", "NULL|400", "NULL|500", "NULL|600", "10|100", "10|101", "11|100",
                "11|101", "20|NULL", "30|NULL", "40|NULL", "50|NULL",
            ],
        ),
        // USING and NATURAL merge their columns as COALESCE(left, right).
        (
            "SELECT * FROM ha a FULL JOIN hb b USING (k) ORDER BY k NULLS FIRST, v NULLS FIRST, w",
            &[
                "NULL|NULL|NULL|n|500",
                "NULL|n|40|NULL|NULL",
                "1|x|10|x|100",
                "1|x|10|x|101",
                "1|x|11|x|100",
                "1|x|11|x|101",
                "2|y|20|q|200",
                "3|z|30|NULL|NULL",
                "4|NULL|NULL|z|400",
                "5|NULL|50|NULL|600",
            ],
        ),
        (
            "SELECT * FROM ha NATURAL FULL JOIN hb ORDER BY v NULLS FIRST, w",
            &[
                "2|q|NULL|200",
                "4|z|NULL|400",
                "NULL|n|NULL|500",
                "5|NULL|NULL|600",
                "1|x|10|100",
                "1|x|10|101",
                "1|x|11|100",
                "1|x|11|101",
                "2|y|20|NULL",
                "3|z|30|NULL",
                "NULL|n|40|NULL",
                "5|NULL|50|NULL",
            ],
        ),
        // WHERE runs after NULL extension, on unmatched rows of both sides.
        (
            "SELECT a.v, b.w FROM ha a FULL JOIN hb b ON a.k = b.k \
             WHERE b.w IS NULL OR b.w > 150 ORDER BY a.v NULLS FIRST, b.w",
            &[
                "NULL|400", "NULL|500", "20|200", "30|NULL", "40|NULL", "50|600",
            ],
        ),
        (
            "SELECT a.v, b.w FROM ha a RIGHT JOIN hb b ON a.k = b.k WHERE a.v IS NULL ORDER BY b.w",
            &["NULL|400", "NULL|500"],
        ),
        // Empty sides.
        (
            "SELECT a.v, e.w FROM ha a FULL JOIN he e ON a.k = e.k ORDER BY a.v",
            &[
                "10|NULL", "11|NULL", "20|NULL", "30|NULL", "40|NULL", "50|NULL",
            ],
        ),
        (
            "SELECT a.v, e.w FROM he e FULL JOIN ha a ON a.k = e.k ORDER BY a.v",
            &[
                "10|NULL", "11|NULL", "20|NULL", "30|NULL", "40|NULL", "50|NULL",
            ],
        ),
        (
            "SELECT a.v, e.w FROM ha a RIGHT JOIN he e ON a.k = e.k ORDER BY a.v",
            &[],
        ),
        (
            "SELECT a.v, e.w FROM he e RIGHT JOIN ha a ON a.k = e.k ORDER BY a.v",
            &[
                "10|NULL", "11|NULL", "20|NULL", "30|NULL", "40|NULL", "50|NULL",
            ],
        ),
        // Conditions on one side only still decide each pair.
        (
            "SELECT a.v, b.w FROM ha a FULL JOIN hb b ON a.k = b.k AND a.v > 15 AND b.w < 450 \
             ORDER BY a.v NULLS FIRST, b.w",
            &[
                "NULL|100", "NULL|101", "NULL|400", "NULL|500", "NULL|600", "10|NULL", "11|NULL",
                "20|200", "30|NULL", "40|NULL", "50|NULL",
            ],
        ),
    ];
    for (query, expected) in cases {
        assert_eq!(
            both_executors(&mut session, &mut engine, query),
            *expected,
            "{query}"
        );
        // Without ORDER BY both executors return the same rows in the same
        // order: probe rows first, then unmatched build rows in row order.
        let unordered = query.split(" ORDER BY ").next().unwrap();
        let mut rows = both_executors(&mut session, &mut engine, unordered);
        rows.sort();
        let mut sorted: Vec<String> = expected.iter().map(|row| row.to_string()).collect();
        sorted.sort();
        assert_eq!(rows, sorted, "{unordered}");
    }

    // PostgreSQL 18.6 plans these after ANALYZE: a right join probes its
    // preserved side and hashes the nullable one; a full join keeps match
    // flags for its hashed rows.
    let query = "SELECT a.v, b.w FROM ha a RIGHT JOIN hb b ON a.k = b.k";
    assert_eq!(
        plan(&mut session, &mut engine, query),
        [
            "Hash Left Join",
            "  Hash Cond: (b.k = a.k)",
            "  ->  Seq Scan on hb b",
            "  ->  Hash",
            "        ->  Seq Scan on ha a"
        ]
    );
    let query = "SELECT a.v, b.w FROM ha a FULL JOIN hb b ON a.k = b.k AND a.v + 90 < b.w";
    assert_eq!(
        plan(&mut session, &mut engine, query),
        [
            "Hash Full Join",
            "  Hash Cond: (a.k = b.k)",
            "  Join Filter: ((a.v + 90) < b.w)",
            "  ->  Seq Scan on ha a",
            "  ->  Hash",
            "        ->  Seq Scan on hb b"
        ]
    );
    // A FULL JOIN condition without an equality is rejected, as in PostgreSQL.
    let error = String::from_utf8_lossy(&session.execute(
        &mut engine,
        "SELECT * FROM ha a FULL JOIN hb b ON a.k < b.k",
        true,
    ))
    .into_owned();
    assert!(
        error.contains(
            "FULL JOIN is only supported with merge-joinable or hash-joinable join conditions"
        ),
        "{error}"
    );
}
