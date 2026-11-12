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
