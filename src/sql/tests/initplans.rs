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
