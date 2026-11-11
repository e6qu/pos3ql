use super::*;

/// (SQLSTATE, message, DETAIL) of each statement's errors, in order.
fn errors(statements: &[&str]) -> Vec<Vec<(String, String, Option<String>)>> {
    let config = test_config("sql-function-results");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    statements
        .iter()
        .map(|sql| {
            super::routine_identity_diagnostics::error_fields(&session.execute(
                &mut engine,
                sql,
                true,
            ))
            .into_iter()
            .map(|(state, message, detail, _)| (state, message, detail))
            .collect()
        })
        .collect()
}

fn mismatch(declared: &str, detail: &str) -> Vec<(String, String, Option<String>)> {
    vec![(
        "42P13".to_string(),
        format!("return type mismatch in function declared to return {declared}"),
        Some(detail.to_string()),
    )]
}

/// With `check_function_bodies` on, CREATE checks the final statement's
/// width and types as PostgreSQL 18.6's `check_sql_stmt_retval` does:
/// assignment conversions are accepted and an untyped literal is `text`.
#[test]
fn create_checks_sql_function_result_columns_and_types() {
    let one = "Final statement must return exactly one column.";
    assert_eq!(
        errors(&[
            "CREATE FUNCTION r1() RETURNS integer LANGUAGE sql AS 'SELECT 1, 2'",
            "CREATE FUNCTION r2() RETURNS integer LANGUAGE sql AS 'SELECT ''a''::text'",
            "CREATE FUNCTION r3() RETURNS integer LANGUAGE sql AS 'SELECT 1::int8'; DROP FUNCTION r3",
            "CREATE FUNCTION r4() RETURNS text LANGUAGE sql AS 'SELECT 1'; DROP FUNCTION r4",
            "CREATE FUNCTION r5() RETURNS integer LANGUAGE sql AS 'SELECT ''1'''",
            "CREATE FUNCTION r6() RETURNS integer LANGUAGE sql AS 'SELECT NULL'",
            "CREATE FUNCTION r7() RETURNS SETOF integer LANGUAGE sql AS 'SELECT 1, 2'",
            "CREATE FUNCTION r8(OUT a int, OUT b text) LANGUAGE sql AS 'SELECT 1'",
            "CREATE FUNCTION r9(OUT a int, OUT b text) LANGUAGE sql AS 'SELECT 1, 2'; DROP FUNCTION r9",
            "CREATE FUNCTION r10() RETURNS TABLE(a int, b text) LANGUAGE sql AS 'SELECT 1, 2.5'; DROP FUNCTION r10",
            "CREATE FUNCTION r11() RETURNS integer LANGUAGE sql BEGIN ATOMIC SELECT 'a'::text; END",
            "CREATE FUNCTION r12() RETURNS integer LANGUAGE sql AS 'SELECT true'",
            "CREATE FUNCTION r13() RETURNS integer[] LANGUAGE sql AS 'SELECT ARRAY[1::int8]'; DROP FUNCTION r13",
            "CREATE FUNCTION r14() RETURNS date LANGUAGE sql AS 'SELECT now()'; DROP FUNCTION r14",
            "CREATE FUNCTION r15() RETURNS void LANGUAGE sql AS 'SELECT 1, 2'; DROP FUNCTION r15",
            "CREATE FUNCTION r16() RETURNS record LANGUAGE sql AS 'SELECT 1, 2'; DROP FUNCTION r16",
            "CREATE TABLE result_rows(a int); \
             CREATE FUNCTION r17() RETURNS integer LANGUAGE sql \
                 AS 'INSERT INTO result_rows VALUES (1) RETURNING a, a'",
            "CREATE FUNCTION r18() RETURNS integer LANGUAGE sql AS 'SELECT 1; SELECT ''x''::text'",
            "CREATE PROCEDURE p1(INOUT a int) LANGUAGE sql AS 'SELECT 1, 2'",
            "CREATE PROCEDURE p2(INOUT a int, OUT b text) LANGUAGE sql AS 'SELECT 1::int8, 2'; \
             DROP PROCEDURE p2",
            "CREATE TYPE pair AS (a int, b text)",
            "CREATE FUNCTION r19() RETURNS pair LANGUAGE sql AS 'SELECT ''x'', 1'",
            "CREATE FUNCTION r20() RETURNS pair LANGUAGE sql AS 'SELECT 1'",
            "CREATE FUNCTION r21() RETURNS pair LANGUAGE sql AS 'SELECT ROW(1, ''x'')::pair'; DROP FUNCTION r21",
        ]),
        [
            mismatch("integer", one),
            mismatch("integer", "Actual return type is text."),
            vec![],
            vec![],
            mismatch("integer", "Actual return type is text."),
            mismatch("integer", "Actual return type is text."),
            mismatch("integer", one),
            mismatch("record", "Final statement returns too few columns."),
            vec![],
            vec![],
            mismatch("integer", "Actual return type is text."),
            mismatch("integer", "Actual return type is boolean."),
            vec![],
            vec![],
            vec![],
            vec![],
            mismatch("integer", one),
            mismatch("integer", "Actual return type is text."),
            mismatch("record", "Final statement returns too many columns."),
            vec![],
            vec![],
            mismatch(
                "pair",
                "Final statement returns text instead of integer at column 1."
            ),
            mismatch("pair", "Final statement returns too few columns."),
            vec![],
        ]
    );
}

/// A body CREATE did not check is checked at its call; a polymorphic result
/// names the type the call resolved, not `anyelement`.
#[test]
fn calls_check_unchecked_and_polymorphic_sql_function_results() {
    assert_eq!(
        errors(&[
            "SET check_function_bodies = off; \
             CREATE FUNCTION late() RETURNS integer LANGUAGE sql AS 'SELECT ''a''::text'; \
             CREATE FUNCTION late_table() RETURNS integer LANGUAGE sql \
                 AS 'CREATE TEMP TABLE late_rows(a int); SELECT a FROM late_rows'; \
             RESET check_function_bodies",
            "SELECT late()",
            "SELECT * FROM late()",
            "SELECT late_table()",
            "CREATE FUNCTION poly(x anyelement) RETURNS anyelement LANGUAGE sql AS 'SELECT 1'",
            "SELECT poly(true)",
            "SELECT poly('a'::text)",
            "CREATE FUNCTION poly_rows(x anyelement) RETURNS SETOF anyelement LANGUAGE sql \
                 AS 'SELECT ''x''::text'",
            "SELECT * FROM poly_rows(1)",
            "CREATE FUNCTION poly_out(x anyelement, OUT a anyelement, OUT b int) LANGUAGE sql \
                 AS 'SELECT x, ''q''::text'",
            "SELECT poly_out(1)",
        ]),
        [
            vec![],
            mismatch("integer", "Actual return type is text."),
            mismatch("integer", "Actual return type is text."),
            vec![],
            vec![],
            mismatch("boolean", "Actual return type is integer."),
            vec![],
            vec![],
            mismatch("integer", "Actual return type is text."),
            vec![],
            mismatch(
                "record",
                "Final statement returns text instead of integer at column 2."
            ),
        ]
    );
}
