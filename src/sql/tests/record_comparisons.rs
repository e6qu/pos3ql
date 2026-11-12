//! Record value comparisons (`record_eq`, `record_cmp`) against comparisons
//! between two row constructors, compared with PostgreSQL 18.6.

use super::compatibility_diagnostics::{diagnostics, with_engine};
use super::*;

fn rows(engine: &mut Engine, budget: &mut Budget, sql: &str) -> Vec<String> {
    let bytes = run_with(engine, budget, sql);
    assert_eq!(
        diagnostics(&bytes)
            .iter()
            .filter(|line| line.starts_with("ERROR"))
            .count(),
        0,
        "{sql}: {:?}",
        diagnostics(&bytes)
    );
    data_rows(&bytes)
}

fn error(engine: &mut Engine, budget: &mut Budget, sql: &str) -> String {
    diagnostics(&run_with(engine, budget, sql))
        .into_iter()
        .find(|line| line.starts_with("ERROR"))
        .unwrap_or_else(|| panic!("{sql} succeeded"))
}

#[test]
fn record_values_treat_null_fields_as_equal_while_row_constructors_do_not() {
    with_engine("record-comparisons", |engine, budget| {
        run_with(engine, budget, "CREATE TABLE rc(a int, b text)");
        run_with(
            engine,
            budget,
            "INSERT INTO rc VALUES (1, NULL), (NULL, NULL), (2, 'x')",
        );
        // Whole-row values compare with record_eq/record_cmp.
        assert_eq!(
            rows(
                engine,
                budget,
                "SELECT rc.* = rc.*, rc.* <> rc.*, rc.* < rc.*, rc <= rc, \
                 rc.* IS DISTINCT FROM rc.* FROM rc ORDER BY a",
            ),
            ["t|f|f|t|f", "t|f|f|t|f", "t|f|f|t|f"]
        );
        // Two row constructors compare field by field.
        assert_eq!(
            rows(
                engine,
                budget,
                "SELECT ROW(rc.*) = ROW(rc.*), (a, b) = (a, b), rc = (a, b) FROM rc ORDER BY a",
            ),
            ["NULL|NULL|t", "t|t|t", "NULL|NULL|t"]
        );
        assert_eq!(
            rows(
                engine,
                budget,
                "SELECT ROW(1, NULL) = ROW(1, NULL), ROW(1, NULL) <> ROW(1, NULL), \
                 ROW(1, NULL) < ROW(2, NULL), (1, NULL::int) < (1, 2), \
                 (2, NULL::int) > (1, 2), ROW(1, NULL) IS DISTINCT FROM ROW(1, NULL), \
                 ROW(1, NULL) IS DISTINCT FROM ROW(1, 2), \
                 ROW(1, NULL) IS NOT DISTINCT FROM ROW(1, NULL), \
                 ((1, NULL::int), 2) IS DISTINCT FROM ((1, NULL::int), 2)",
            ),
            ["NULL|NULL|t|NULL|t|f|t|t|f"]
        );
        // A record value against a row constructor, and two record values.
        assert_eq!(
            rows(
                engine,
                budget,
                "SELECT rc = ROW(1, NULL::text), rc.* = ROW(1, NULL::text), \
                 (SELECT x FROM rc x WHERE a = 1) = (SELECT x FROM rc x WHERE a = 1) \
                 FROM rc ORDER BY a",
            ),
            ["t|t|t", "f|f|t", "f|f|t"]
        );
        assert_eq!(
            rows(
                engine,
                budget,
                "SELECT x = y, x < y, x <> y, x IS DISTINCT FROM y \
                 FROM (SELECT rc x, rc y FROM rc) s ORDER BY 1",
            ),
            ["t|f|f|f", "t|f|f|f", "t|f|f|f"]
        );
        assert_eq!(
            rows(
                engine,
                budget,
                "SELECT (SELECT ROW(1, NULL::int)) < (SELECT ROW(1, 2)), \
                 (SELECT ROW(1, 2)) < (SELECT ROW(1, NULL::int)), \
                 (SELECT ROW(1, 'NaN'::float8)) = (SELECT ROW(1, 'NaN'::float8)), \
                 NULLIF(ROW(1, NULL::int), ROW(1, NULL::int)) IS NULL, \
                 CASE ROW(1, NULL::int) WHEN ROW(1, NULL::int) THEN 'y' ELSE 'n' END",
            ),
            ["f|t|t|t|y"]
        );
        // Membership: row-constructor lists compare field by field, record
        // members as values.
        assert_eq!(
            rows(
                engine,
                budget,
                "SELECT (1, NULL) IN ((1, NULL), (2, 3)), \
                 (1, NULL::int) IN (SELECT 1, NULL::int), \
                 (SELECT (1, NULL::int)) IN (SELECT (1, NULL::int)), \
                 (SELECT (1, NULL::int)) = ANY (SELECT (1, NULL::int)), \
                 ROW(1, NULL::int) = ANY (SELECT 1, NULL::int), ROW(1) IN (SELECT 1)",
            ),
            ["NULL|NULL|t|t|NULL|t"]
        );
        assert_eq!(
            rows(
                engine,
                budget,
                "SELECT rc IN (SELECT rc FROM rc), rc.* = ANY (SELECT rc FROM rc), \
                 rc.* IN (rc.*) FROM rc ORDER BY a",
            ),
            ["t|t|t", "t|t|t", "t|t|t"]
        );
        assert_eq!(
            rows(
                engine,
                budget,
                "SELECT count(*) FROM rc r1 JOIN rc r2 ON r1 = r2 \
                 UNION ALL SELECT count(*) FROM rc r1 JOIN rc r2 ON (r1.a, r1.b) = (r2.a, r2.b)",
            ),
            ["3", "1"]
        );
        // record_eq requires identical field types and widths.
        for (sql, expected) in [
            (
                "SELECT rc = ROW(1, NULL) FROM rc",
                "ERROR 42804 cannot compare dissimilar column types text and unknown at record column 2",
            ),
            (
                "SELECT (SELECT ROW(1, NULL)) = ROW(1, NULL)",
                "ERROR 42883 could not identify an equality operator for type unknown",
            ),
            (
                "SELECT ((1, NULL), 2) = ((1, NULL), 2)",
                "ERROR 42883 could not identify an equality operator for type unknown",
            ),
            (
                "SELECT (SELECT ROW(1, 2)) = ROW(1, 'a'::text)",
                "ERROR 42804 cannot compare dissimilar column types integer and text at record column 2",
            ),
            (
                "SELECT (SELECT ROW(1, 2)) = (SELECT ROW(1, 2, 3))",
                "ERROR 42804 cannot compare record types with different numbers of columns",
            ),
        ] {
            assert_eq!(error(engine, budget, sql), expected, "{sql}");
        }
        assert_eq!(
            rows(
                engine,
                budget,
                "SELECT (SELECT ROW(1, 2)) = (SELECT ROW(2, 2, 3))",
            ),
            ["f"]
        );
    });
}

#[test]
fn sorting_grouping_and_set_operations_order_records_by_field_values() {
    with_engine("record-ordering", |engine, budget| {
        run_with(engine, budget, "CREATE TABLE ro(a int, b numeric)");
        run_with(
            engine,
            budget,
            "INSERT INTO ro VALUES (10, 1.0), (9, 1.00), (2, NULL), (10, 1.00), (100, 2), (NULL, 1)",
        );
        let ordered = [
            "(2,)",
            "(9,1.00)",
            "(10,1.0)",
            "(10,1.00)",
            "(100,2)",
            "(,1)",
        ];
        assert_eq!(
            rows(engine, budget, "SELECT ROW(a, b) FROM ro ORDER BY 1"),
            ordered
        );
        assert_eq!(
            rows(
                engine,
                budget,
                "SELECT array_agg(ROW(a, b) ORDER BY ROW(a, b)) FROM ro"
            ),
            [r#"{"(2,)","(9,1.00)","(10,1.0)","(10,1.00)","(100,2)","(,1)"}"#]
        );
        // Equal fields spelled differently form one group, represented by
        // its first input row.
        let distinct = ["(2,)", "(9,1.00)", "(10,1.0)", "(100,2)", "(,1)"];
        assert_eq!(
            rows(
                engine,
                budget,
                "SELECT DISTINCT ROW(a, b) FROM ro ORDER BY 1"
            ),
            distinct
        );
        assert_eq!(
            rows(
                engine,
                budget,
                "SELECT ROW(a, b), count(*) FROM ro GROUP BY 1 ORDER BY 1"
            ),
            ["(2,)|1", "(9,1.00)|1", "(10,1.0)|2", "(100,2)|1", "(,1)|1"]
        );
        assert_eq!(
            rows(
                engine,
                budget,
                "SELECT ROW(a, b) FROM ro UNION SELECT ROW(a, b) FROM ro ORDER BY 1"
            ),
            distinct
        );
        assert_eq!(
            rows(
                engine,
                budget,
                "WITH RECURSIVE t(r) AS (SELECT ROW(1.0) UNION SELECT ROW(1.00) FROM t) \
                 SELECT * FROM t"
            ),
            ["(1.0)"]
        );
        assert_eq!(
            rows(
                engine,
                budget,
                "SELECT x FROM (VALUES (ARRAY[ROW(1), NULL]), (ARRAY[ROW(1), ROW(0)])) v(x) \
                 ORDER BY x"
            ),
            ["{(1),(0)}", "{(1),NULL}"]
        );
        // A window query's ORDER BY reports a failed comparison instead of
        // treating the pair as equal.
        assert_eq!(
            error(
                engine,
                budget,
                "SELECT row_number() OVER (), r FROM (VALUES (ROW(1)), (ROW('a'::text))) v(r) \
                 ORDER BY r"
            ),
            "ERROR 42804 cannot compare dissimilar column types integer and text at record column 1"
        );
    });
}

#[test]
fn named_composite_arrays_compare_catalog_fields_not_element_text() {
    with_engine("composite-array-comparisons", |engine, budget| {
        run_with(engine, budget, "CREATE TYPE cac AS (a int, b numeric)");
        run_with(engine, budget, "CREATE TABLE cat(id int, x cac[])");
        run_with(
            engine,
            budget,
            "INSERT INTO cat VALUES (1, ARRAY[ROW(10, 1)::cac]), (2, ARRAY[ROW(9, 1)::cac]), \
             (3, ARRAY[ROW(9, 1.00)::cac])",
        );
        // Text order would put "(10,1)" first and keep 1 and 1.00 apart.
        assert_eq!(
            rows(engine, budget, "SELECT id FROM cat ORDER BY x, id"),
            ["2", "3", "1"]
        );
        assert_eq!(
            rows(
                engine,
                budget,
                "SELECT ARRAY[ROW(10, 1)::cac] < ARRAY[ROW(9, 1)::cac], \
                 ARRAY[ROW(9, 1.0)::cac] = ARRAY[ROW(9, 1.00)::cac]",
            ),
            ["f|t"]
        );
        assert_eq!(
            rows(
                engine,
                budget,
                "SELECT count(DISTINCT x), max(x), min(x) FROM cat"
            ),
            ["2|{\"(10,1)\"}|{\"(9,1.00)\"}"]
        );
        assert_eq!(
            rows(
                engine,
                budget,
                "SELECT DISTINCT x, count(*) OVER () FROM cat ORDER BY x"
            ),
            ["{\"(9,1)\"}|3", "{\"(10,1)\"}|3"]
        );
        assert_eq!(
            rows(
                engine,
                budget,
                "SELECT array_position(ARRAY[ROW(9, 1.0)::cac], ROW(9, 1)::cac), \
                 ARRAY[ROW(9, 1.0)::cac] @> ARRAY[ROW(9, 1)::cac], \
                 array_append(ARRAY[ROW(9, 1.0)::cac], ROW(1, 1)::cac)",
            ),
            ["1|t|{\"(9,1.0)\",\"(1,1)\"}"]
        );
    });
}
