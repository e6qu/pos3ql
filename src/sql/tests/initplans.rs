use super::aggregate_levels::rows;
use super::*;

/// Expected values were read from PostgreSQL 18.6 (`initdb --locale=C`).
#[test]
fn uncorrelated_subqueries_run_when_first_read_after_one_time_filters() {
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    for sql in [
        "CREATE TABLE ip_t (id int)",
        "CREATE TABLE ip_e (id int)",
        "CREATE TABLE ip_z (x int)",
        "INSERT INTO ip_t VALUES (1), (2), (3)",
        "INSERT INTO ip_z VALUES (0)",
    ] {
        run_txn(&mut e, &mut b, &mut t, sql);
    }
    let mut check = |sql: &str, expected: &[&str]| {
        assert_eq!(rows(&mut e, &mut b, &mut t, sql), expected, "{sql}");
    };
    let zero = "ERROR 22012: division by zero";
    // No row reads the subquery, so it never runs.
    check("SELECT (SELECT 1/x FROM ip_z) FROM ip_e", &[]);
    check("SELECT (SELECT 1/x FROM ip_z) FROM ip_e ORDER BY 1", &[]);
    check("SELECT ARRAY(SELECT 1/x FROM ip_z) FROM ip_e", &[]);
    check(
        "SELECT CASE WHEN id > 100 THEN (SELECT 1/x FROM ip_z) END FROM ip_t",
        &["NULL", "NULL", "NULL"],
    );
    check("SELECT (SELECT 1/x FROM ip_z) FROM ip_t", &[zero]);
    // A condition that reads no column runs once before the scan.
    check(
        "SELECT * FROM ip_e WHERE (SELECT 1/x FROM ip_z) > 0",
        &[zero],
    );
    check(
        "SELECT * FROM ip_t WHERE id > 5 AND (SELECT 1/x FROM ip_z) > 0",
        &[zero],
    );
    check(
        "SELECT * FROM ip_t WHERE (SELECT x FROM ip_z) > 0 AND 1/(id - 1) > 0",
        &[],
    );
    check(
        "SELECT 1/(id - 1) FROM ip_t WHERE (SELECT x FROM ip_z) > 0",
        &[],
    );
    check(
        "SELECT count(*) FROM ip_t WHERE (SELECT x FROM ip_z) > 0 GROUP BY id",
        &[],
    );
    check(
        "SELECT id, (SELECT max(x) FROM ip_z) FROM ip_t ORDER BY id LIMIT 1",
        &["1|0"],
    );
}

/// `IN`, `ANY` and `ALL` lists run when first read, in local and object-store
/// mode. Expected values were read from PostgreSQL 18.6 (`initdb --locale=C`).
#[test]
fn uncorrelated_subquery_lists_run_when_first_read() {
    for object_store in [false, true] {
        let name = format!("initplan-lists-{object_store}");
        let mut config = test_config(&name);
        if object_store {
            config.object_store_on = true;
            config.object_store_sim = true;
            config.object_store_bucket = format!("{name}-{}", std::process::id());
            crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        }
        let mut b = test_engine_budget(&config, 1 << 29);
        let mut e = Engine::new(&config, &mut b).unwrap();
        let mut t = TxnState::new_with_config(&mut b, &config).unwrap();
        for sql in [
            "CREATE TABLE il_t (id int, s text)",
            "CREATE TABLE il_e (id int)",
            "CREATE TABLE il_z (x int, s text)",
            "INSERT INTO il_t VALUES (1, 'a'), (2, 'b'), (3, NULL)",
            "INSERT INTO il_z VALUES (0, 'a'), (NULL, 'B')",
        ] {
            run_txn(&mut e, &mut b, &mut t, sql);
        }
        let mut check = |sql: &str, expected: &[&str]| {
            assert_eq!(
                rows(&mut e, &mut b, &mut t, sql),
                expected,
                "{sql} (object store: {object_store})"
            );
        };
        let zero = "ERROR 22012: division by zero";
        // No row reaches the list, so its subquery never runs.
        for sql in [
            "SELECT * FROM il_t WHERE id > 5 AND id IN (SELECT 1/x FROM il_z)",
            "SELECT * FROM il_t WHERE id > 5 AND id NOT IN (SELECT 1/x FROM il_z)",
            "SELECT * FROM il_t WHERE id > 5 AND id > ALL (SELECT 1/x FROM il_z)",
            "SELECT id FROM il_t WHERE id > 5 AND id IN (SELECT 1/x FROM il_z) ORDER BY id",
            "SELECT id FROM il_t WHERE id > 5 AND id IN (SELECT 1/x FROM il_z) GROUP BY id",
            "SELECT id IN (SELECT 1/x FROM il_z) FROM il_e",
        ] {
            check(sql, &[]);
        }
        check(
            "SELECT CASE WHEN id > 10 THEN id IN (SELECT 1/x FROM il_z) END FROM il_t",
            &["NULL", "NULL", "NULL"],
        );
        check("SELECT id IN (SELECT 1/x FROM il_z) FROM il_t", &[zero]);
        // Each list keeps its members, NULL member, type and collation.
        check(
            "SELECT id, id IN (SELECT x FROM il_z), id NOT IN (SELECT x FROM il_z), \
             id > ANY (SELECT x FROM il_z), id < ALL (SELECT x + 5 FROM il_z WHERE x IS NOT NULL) \
             FROM il_t ORDER BY id",
            &["1|NULL|NULL|t|t", "2|NULL|NULL|t|t", "3|NULL|NULL|t|t"],
        );
        check(
            "SELECT id, s IN (SELECT s FROM il_z), \
             s NOT IN (SELECT s FROM il_z WHERE s IS NOT NULL) FROM il_t ORDER BY id",
            &["1|t|f", "2|f|t", "3|NULL|NULL"],
        );
        check(
            "SELECT id, (id, s) IN (SELECT x + 1, s FROM il_z) FROM il_t ORDER BY id",
            &["1|t", "2|f", "3|NULL"],
        );
        check(
            "SELECT id FROM il_t WHERE s COLLATE \"C\" IN (SELECT lower(s) FROM il_z) ORDER BY id",
            &["1", "2"],
        );
        check(
            "SELECT id, NULL::int NOT IN (SELECT x FROM il_z WHERE false) FROM il_t ORDER BY id",
            &["1|t", "2|t", "3|t"],
        );
        // The operand is typed against the list while the query is analysed.
        check(
            "SELECT 'zz' IN (SELECT x FROM il_z) FROM il_e",
            &["ERROR 22P02: invalid input syntax for type integer: \"zz\""],
        );
        check(
            "SELECT ('zz', 1) IN (SELECT x, 1 FROM il_z) FROM il_e",
            &["ERROR 22P02: invalid input syntax for type integer: \"zz\""],
        );
        check(
            "SELECT 'zz' = (SELECT x FROM il_z) FROM il_e",
            &["ERROR 22P02: invalid input syntax for type integer: \"zz\""],
        );
        check(
            "SELECT * FROM il_e WHERE id IN (SELECT s FROM il_z)",
            &["ERROR 42883: operator does not exist: integer = text"],
        );
        drop(t);
        drop(e);
        if object_store {
            crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        }
    }
}

/// Sorted and grouped queries keep their uncorrelated subqueries, and a
/// subquery over a set-returning function of constants is uncorrelated.
/// Expected values were read from PostgreSQL 18.6 (`initdb --locale=C`).
#[test]
fn sorted_and_grouped_queries_keep_uncorrelated_subqueries() {
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    for sql in [
        "CREATE TABLE ss_t (id int, s text)",
        "CREATE TABLE ss_z (x int)",
        "INSERT INTO ss_t SELECT g, 'r' || g FROM generate_series(1, 6) g",
        "INSERT INTO ss_z VALUES (0), (2)",
    ] {
        run_txn(&mut e, &mut b, &mut t, sql);
    }
    let mut check = |sql: &str, expected: &[&str]| {
        assert_eq!(rows(&mut e, &mut b, &mut t, sql), expected, "{sql}");
    };
    check(
        "SELECT id FROM ss_t WHERE id > 60 AND id IN (SELECT 1/x FROM ss_z) ORDER BY id",
        &[],
    );
    check(
        "SELECT id FROM ss_t WHERE id % 3 IN (SELECT x FROM ss_z) ORDER BY id DESC",
        &["6", "5", "3", "2"],
    );
    check(
        "SELECT id FROM ss_t WHERE id > (SELECT min(x) FROM ss_z) ORDER BY s DESC LIMIT 3",
        &["6", "5", "4"],
    );
    check(
        "SELECT id % 2, count(*) FROM ss_t WHERE id IN (SELECT x FROM ss_z) OR id > 4 \
         GROUP BY id % 2 ORDER BY 1",
        &["0|2", "1|1"],
    );
    check(
        "SELECT id FROM ss_t WHERE id IN (SELECT g FROM generate_series(2, 3) g) ORDER BY id DESC",
        &["3", "2"],
    );
    check(
        "SELECT id, (SELECT max(g) FROM generate_series(1, id) g) FROM ss_t WHERE id < 3 \
         ORDER BY id DESC",
        &["2|2", "1|1"],
    );
    check(
        "SELECT id FROM ss_t WHERE id = 1 OR id IN (SELECT 1/x FROM ss_z) ORDER BY id",
        &["ERROR 22012: division by zero"],
    );
}

/// A row compared with a subquery reads the subquery's one row, and an
/// unknown-type literal field takes the type of the field it is compared
/// with. Expected values were read from PostgreSQL 18.6 (`initdb --locale=C`).
#[test]
fn rows_compare_with_row_subqueries_and_literal_fields() {
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    for sql in [
        "CREATE TABLE rc_o (id int, s text)",
        "CREATE TABLE rc_i (v int, t text)",
        "INSERT INTO rc_o VALUES (1, 'a'), (2, 'b'), (3, NULL)",
        "INSERT INTO rc_i VALUES (1, 'a'), (2, 'x')",
    ] {
        run_txn(&mut e, &mut b, &mut t, sql);
    }
    let mut check = |sql: &str, expected: &[&str]| {
        assert_eq!(rows(&mut e, &mut b, &mut t, sql), expected, "{sql}");
    };
    check(
        "SELECT id, (id, s) = (SELECT v, t FROM rc_i WHERE v = rc_o.id) FROM rc_o ORDER BY id",
        &["1|t", "2|f", "3|NULL"],
    );
    check(
        "SELECT id FROM rc_o WHERE ROW(id, 1) < (SELECT v, 2 FROM rc_i WHERE v = rc_o.id) \
         ORDER BY id",
        &["1", "2"],
    );
    check(
        "SELECT (1, 2) = (SELECT v, v FROM rc_i)",
        &["ERROR 21000: more than one row returned by a subquery used as an expression"],
    );
    check(
        "SELECT (1, 2) = (SELECT v FROM rc_i WHERE v = 1)",
        &["ERROR 42601: subquery has too few columns"],
    );
    check("SELECT (1, 2) = (SELECT 1, 2 WHERE false)", &["NULL"]);
    check("SELECT (1, '2') = (1, 2), ('1', 2) < (2, 2)", &["t|t"]);
    check(
        "SELECT (1, 'zz') = (SELECT 1, 2)",
        &["ERROR 22P02: invalid input syntax for type integer: \"zz\""],
    );
}

/// A correlated subquery run for each of 100000 input rows reclaims its
/// allocations after each row folds into a plain-data aggregate or passes the
/// condition, so a 2 MiB statement arena holds the whole statement.
#[test]
fn per_row_correlated_subqueries_reclaim_their_allocations() {
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    for sql in [
        "CREATE TABLE pr_big (id int)",
        "CREATE TABLE pr_z (x int)",
        "INSERT INTO pr_big SELECT g FROM generate_series(1, 100) g",
        "INSERT INTO pr_z VALUES (1)",
    ] {
        run_txn(&mut e, &mut b, &mut t, sql);
    }
    let subquery = "(SELECT x FROM pr_z WHERE x <= pr_big.id)";
    let from = "pr_big, generate_series(1, 1000) g";
    for (sql, expected) in [
        (
            format!("SELECT sum({subquery}) FROM {from}"),
            vec!["100000"],
        ),
        (
            format!("SELECT max({subquery}) FROM {from} GROUP BY g % 2 ORDER BY 1"),
            vec!["1", "1"],
        ),
        (
            format!("SELECT count(*) FROM {from} WHERE {subquery} = 1 GROUP BY g % 2"),
            vec!["50000", "50000"],
        ),
    ] {
        assert_eq!(rows(&mut e, &mut b, &mut t, &sql), expected, "{sql}");
    }
}

/// Data-modifying statements run their subqueries when first read, and every
/// one reads as of the statement's start. Expected values were read from
/// PostgreSQL 18.6 (`initdb --locale=C`).
#[test]
fn dml_subqueries_run_when_first_read_as_of_the_statement() {
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    for sql in [
        "CREATE TABLE dq_t (id int PRIMARY KEY, v int)",
        "CREATE TABLE dq_z (x int, y int)",
        "CREATE TABLE dq_zero (x int)",
        "INSERT INTO dq_t VALUES (1, 10), (2, 20), (3, 30)",
        "INSERT INTO dq_z VALUES (1, 5), (3, 7)",
        "INSERT INTO dq_zero VALUES (0)",
    ] {
        run_txn(&mut e, &mut b, &mut t, sql);
    }
    let mut check = |sql: &str, expected: &[&str]| {
        assert_eq!(rows(&mut e, &mut b, &mut t, sql), expected, "{sql}");
    };
    let zero = "ERROR 22012: division by zero";
    check(
        "UPDATE dq_t SET v = (SELECT 1/x FROM dq_zero) WHERE id > 100 RETURNING id",
        &[],
    );
    check(
        "DELETE FROM dq_t WHERE id > 100 AND v IN (SELECT 1/x FROM dq_zero) RETURNING id",
        &[],
    );
    check(
        "UPDATE dq_t SET v = v WHERE id > 100 AND (SELECT 1/x FROM dq_zero) > 0 RETURNING id",
        &[zero],
    );
    check(
        "DELETE FROM dq_t WHERE id / 0 > 1 AND (SELECT count(*) FROM dq_zero) > 5 RETURNING id",
        &[],
    );
    check(
        "INSERT INTO dq_t SELECT 9, 9 WHERE 1 = ANY (SELECT 1/x FROM dq_zero) AND false RETURNING id",
        &[],
    );
    // A subquery first run after the statement wrote rows reads them as
    // they were when it started.
    check(
        "UPDATE dq_t SET v = CASE WHEN id = 3 THEN (SELECT sum(v) FROM dq_t) ELSE v + 100 END RETURNING id, v",
        &["1|110", "2|120", "3|60"],
    );
    check(
        "INSERT INTO dq_t VALUES (7, 0), (8, (SELECT count(*) FROM dq_t)) RETURNING id, v",
        &["7|0", "8|3"],
    );
    // Correlated subqueries read the target and FROM rows.
    check(
        "UPDATE dq_t SET v = (SELECT y FROM dq_z WHERE x = dq_t.id) WHERE EXISTS (SELECT 1 FROM dq_z WHERE x = dq_t.id) RETURNING id, v",
        &["1|5", "3|7"],
    );
    check(
        "UPDATE dq_t SET v = s.y + 1 FROM dq_z s WHERE s.x = dq_t.id AND dq_t.v >= (SELECT min(y) FROM dq_z WHERE x = s.x) RETURNING dq_t.id, dq_t.v",
        &["1|6", "3|8"],
    );
}

/// An uncorrelated subquery nested in a correlated one runs once for the
/// statement, as PostgreSQL's initPlan does, rather than once per outer row:
/// the value is kept in one slot, so 100000 outer rows fit the statement
/// arena. Expected values were read from PostgreSQL 18.6 (`initdb --locale=C`).
#[test]
fn nested_uncorrelated_subqueries_run_once_per_statement() {
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    for sql in [
        "CREATE TABLE nu_big (id int)",
        "CREATE TABLE nu_z (x int)",
        "CREATE SEQUENCE nu_s",
        "INSERT INTO nu_big SELECT g FROM generate_series(1, 100) g",
        "INSERT INTO nu_z VALUES (1)",
    ] {
        run_txn(&mut e, &mut b, &mut t, sql);
    }
    let from = "nu_big, generate_series(1, 1000) g";
    for (sql, expected) in [
        (
            format!(
                "SELECT sum((SELECT x + (SELECT max(x) FROM nu_z) FROM nu_z WHERE x <= nu_big.id)) FROM {from}"
            ),
            vec!["200000"],
        ),
        (
            format!("SELECT sum((SELECT nu_big.id + (SELECT max(x) FROM nu_z))) FROM {from}"),
            vec!["5150000"],
        ),
        (
            format!(
                "SELECT count(*) FROM {from} WHERE EXISTS (SELECT 1 FROM nu_z WHERE x <= nu_big.id AND x IN (SELECT x FROM nu_z))"
            ),
            vec!["100000"],
        ),
        (
            format!(
                "SELECT max((SELECT x + (SELECT (SELECT max(x) FROM nu_z) FROM nu_z WHERE x < g) FROM nu_z WHERE x <= nu_big.id)) FROM {from}"
            ),
            vec!["2"],
        ),
        (
            "SELECT id, (SELECT x + (SELECT nextval('nu_s')) FROM nu_z WHERE x <= nu_big.id) FROM nu_big WHERE id <= 3 ORDER BY id".to_owned(),
            vec!["1|2", "2|2", "3|2"],
        ),
    ] {
        assert_eq!(rows(&mut e, &mut b, &mut t, &sql), expected, "{sql}");
    }
}

/// Aggregates whose running values are text or numeric, and derived tables
/// kept in local mode, reclaim each input row's evaluation scratch: 20000
/// rows of 1 KiB scratch each fit the 2 MiB statement arena. Expected values
/// were read from PostgreSQL 18.6 (`initdb --locale=C`).
#[test]
fn held_aggregate_values_and_derived_tables_reclaim_row_scratch() {
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    let rows_of = "generate_series(1, 20000) g";
    for (sql, expected) in [
        (
            format!(
                "SELECT length(max(repeat('x', 1000) || g)), right(max(repeat('x', 1000) || g), 4), min(g::text) FROM {rows_of}"
            ),
            vec!["1004|9999|1"],
        ),
        (
            format!(
                "SELECT sum(length(repeat('x', 1000))::numeric + g / 3.0), avg(length(repeat('y', 1000) || g)) FROM {rows_of}"
            ),
            vec!["86670000.00000000000000000000|1004.4447000000000000"],
        ),
        (
            format!(
                "SELECT g % 2, right(max(repeat('z', 1000) || g), 5), variance(length(repeat('v', 1000) || g)) FROM {rows_of} GROUP BY 1 ORDER BY 1"
            ),
            vec![
                "0|z9998|0.36880087008700870087",
                "1|z9999|0.36995674567456745675",
            ],
        ),
        (
            format!(
                "SELECT count(*), max(v) FROM (SELECT length(repeat('x', 1000) || g) AS v FROM {rows_of} LIMIT 30000) d"
            ),
            vec!["20000|1005"],
        ),
    ] {
        assert_eq!(rows(&mut e, &mut b, &mut t, &sql), expected, "{sql}");
    }
}

/// An uncorrelated subquery keeps its statement slot when the query around it
/// is rewritten for each outer row (an `IN` operand over a merged FROM
/// subquery), sits in a set-operation branch, a `LATERAL` FROM subquery, a
/// FROM function's arguments or a row-valued `IN`, so it runs once and 20000
/// outer rows share one value. Expected values were read from PostgreSQL 18.6 (`initdb --locale=C`).
#[test]
fn rewritten_and_lateral_nested_subqueries_share_their_slot() {
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    for sql in [
        "CREATE TABLE rw_big (id int)",
        "CREATE TABLE rw_z (x int)",
        "CREATE SEQUENCE rw_s",
        "INSERT INTO rw_big SELECT g FROM generate_series(1, 100) g",
        "INSERT INTO rw_z VALUES (1), (2)",
    ] {
        run_txn(&mut e, &mut b, &mut t, sql);
    }
    for (sql, expected) in [
        (
            "SELECT sum((SELECT count(*) FROM (SELECT x FROM rw_z) d WHERE d.x <= rw_big.id \
             AND d.x IN (SELECT max(x) FROM rw_z))) FROM rw_big, generate_series(1, 200) g",
            vec!["19800"],
        ),
        (
            "SELECT id, (SELECT count(*) FROM (SELECT x FROM rw_z) d WHERE d.x <= rw_big.id \
             AND d.x IN (SELECT nextval('rw_s') + 1)) FROM rw_big WHERE id <= 3 ORDER BY id",
            vec!["1|0", "2|1", "3|1"],
        ),
        (
            "SELECT id, (SELECT x FROM rw_z WHERE x <= rw_big.id UNION \
             SELECT (SELECT nextval('rw_s'))::int ORDER BY 1 DESC LIMIT 1) \
             FROM rw_big WHERE id <= 3 ORDER BY id",
            vec!["1|2", "2|2", "3|2"],
        ),
        (
            "SELECT id, l.m FROM rw_big, LATERAL (SELECT max(x) + (SELECT nextval('rw_s')) m \
             FROM rw_z WHERE x <= rw_big.id) l WHERE id <= 3 ORDER BY id",
            vec!["1|4", "2|5", "3|5"],
        ),
        (
            "SELECT id, (SELECT max(x) FROM rw_z WHERE (x, 4) IN \
             (SELECT rw_big.id, (SELECT nextval('rw_s'))::int)) \
             FROM rw_big WHERE id <= 3 ORDER BY id",
            vec!["1|1", "2|2", "3|NULL"],
        ),
        (
            "SELECT id, g FROM rw_big, LATERAL generate_series(id, id + (SELECT nextval('rw_s'))::int % 1) g \
             WHERE id <= 2 ORDER BY id",
            vec!["1|1", "2|2"],
        ),
        ("SELECT last_value FROM rw_s", vec!["5"]),
    ] {
        assert_eq!(rows(&mut e, &mut b, &mut t, sql), expected, "{sql}");
    }
}

/// A scalar or `ARRAY` subquery column is described with its declared type,
/// however its value is computed. Type OIDs were read from PostgreSQL 18.6.
#[test]
fn subquery_columns_are_described_with_their_declared_types() {
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    for sql in [
        "CREATE TABLE sd_t (id int)",
        "CREATE TABLE sd_z (x int)",
        "INSERT INTO sd_t VALUES (1), (2)",
        "INSERT INTO sd_z VALUES (1), (2)",
    ] {
        run_txn(&mut e, &mut b, &mut t, sql);
    }
    for (sql, expected) in [
        ("SELECT (SELECT 1.5 FROM sd_z LIMIT 1)", vec![1700]),
        ("SELECT (SELECT max(x::numeric) FROM sd_z)", vec![1700]),
        ("SELECT (SELECT avg(x) FROM sd_z WHERE x > 10)", vec![1700]),
        (
            "SELECT id, (SELECT sum(x::bigint) FROM sd_z WHERE x <= t.id) FROM sd_t t",
            vec![23, 1700],
        ),
        (
            "SELECT ARRAY(SELECT x::numeric FROM sd_z WHERE x <= t.id) FROM sd_t t",
            vec![1231],
        ),
        (
            "SELECT (SELECT string_agg(x::text, ',') FROM sd_z WHERE x <= t.id) FROM sd_t t",
            vec![25],
        ),
    ] {
        let bytes = run_txn_bytes(&mut e, &mut b, &mut t, sql);
        assert_eq!(row_description_type_oids(&bytes), expected, "{sql}");
    }
}

/// Buffering aggregates (`DISTINCT`, aggregate `ORDER BY`, collecting,
/// ordered-set and range aggregates) keep their buffers apart from each input
/// row's evaluation scratch: 5000 to 20000 rows of 1 KiB scratch each fit
/// the 2 MiB statement arena. Expected values were read from PostgreSQL 18.6
/// (`initdb --locale=C`).
#[test]
fn buffering_aggregates_reclaim_row_scratch() {
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    let rows_of = "generate_series(1, 20000) g";
    let few = "generate_series(1, 5000) g";
    for (sql, expected) in [
        (
            format!(
                "SELECT length(string_agg(left(repeat('x', 1000) || g, 1), '')), \
                 count(DISTINCT length(repeat('z', 1000) || g)), \
                 sum(DISTINCT length(repeat('s', 1000) || g) % 10) FROM {rows_of}"
            ),
            vec!["20000|5|15"],
        ),
        (
            format!(
                "SELECT cardinality(array_agg(length(repeat('y', 1000) || g) ORDER BY g)), \
                 length(jsonb_agg(length(repeat('j', 1000) || g) % 2)::text) \
                 FROM {few}"
            ),
            vec!["5000|15000"],
        ),
        (
            format!(
                "SELECT percentile_disc(0.5) WITHIN GROUP (ORDER BY length(repeat('w', 1000) || g)), \
                 range_agg(int4range(length(repeat('r', 1000) || g), length(repeat('r', 1000) || g) + 1)) \
                 FROM {few}"
            ),
            vec!["1004|{[1001,1005)}"],
        ),
        (
            format!(
                "SELECT g % 2, count(DISTINCT length(repeat('z', 1000) || g)), \
                 cardinality(array_agg(DISTINCT length(repeat('y', 1000) || g) % 5)) \
                 FROM {rows_of} GROUP BY 1 ORDER BY 1"
            ),
            vec!["0|5|5", "1|5|5"],
        ),
        (
            "SELECT array_agg(DISTINCT x), json_agg(DISTINCT x) FROM (VALUES (2), (NULL), (1), (NULL), (2)) v(x)".to_owned(),
            vec!["{1,2,NULL}|[1, 2, null]"],
        ),
        (
            "SELECT jsonb_object_agg(k, v) FROM (VALUES ('b', 1), ('a', 2), ('b', 3), ('aa', 4)) t(k, v)".to_owned(),
            vec![r#"{"a": 2, "b": 3, "aa": 4}"#],
        ),
        (
            "SELECT xmlelement(name e, NULL, NULL), xmlelement(name e, xmlattributes(1 AS x), NULL)".to_owned(),
            vec![r#"<e/>|<e x="1"/>"#],
        ),
    ] {
        assert_eq!(rows(&mut e, &mut b, &mut t, &sql), expected, "{sql}");
    }
}
