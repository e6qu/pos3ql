use super::*;

/// PostgreSQL checks `EXECUTE` on the function behind each operator, cast,
/// membership test and grouping key when a plan starts, so a revoked
/// built-in fails even when no row reaches the expression.
#[test]
fn revoked_builtins_are_checked_behind_operators_casts_and_grouping() {
    let (mut engine, mut budget) = test_engine();
    let output = run_with(
        &mut engine,
        &mut budget,
        "CREATE ROLE caller; CREATE TABLE filled(a integer, n numeric); \
         INSERT INTO filled VALUES (1, 2.5); CREATE TABLE empty(a integer); \
         GRANT ALL ON filled, empty TO caller; \
         REVOKE EXECUTE ON FUNCTION int4pl(integer, integer) FROM PUBLIC; \
         REVOKE EXECUTE ON FUNCTION int4eq(integer, integer) FROM PUBLIC; \
         REVOKE EXECUTE ON FUNCTION int4(numeric) FROM PUBLIC",
    );
    assert!(
        error_sqlstates(&output).is_empty(),
        "{}",
        String::from_utf8_lossy(&output)
    );
    for (sql, denied) in [
        ("SELECT 1 + 1", "int4pl"),
        ("SELECT a + 1 FROM empty", "int4pl"),
        ("SELECT a FROM filled WHERE a IN (1, 2)", "int4eq"),
        ("SELECT a IS DISTINCT FROM 1 FROM filled", "int4eq"),
        ("SELECT CASE a WHEN 1 THEN 'one' END FROM filled", "int4eq"),
        ("SELECT n::integer FROM filled", "int4"),
        ("SELECT DISTINCT a FROM filled", "int4eq"),
        ("SELECT a FROM filled UNION SELECT a FROM empty", "int4eq"),
        ("UPDATE empty SET a = a + 1", "int4pl"),
        ("DELETE FROM empty WHERE a = 1", "int4eq"),
    ] {
        let output = run_with(&mut engine, &mut budget, &format!("SET ROLE caller; {sql}"));
        assert_eq!(error_sqlstates(&output), ["42501"], "{sql}");
        assert!(
            String::from_utf8_lossy(&output)
                .contains(&format!("permission denied for function {denied}")),
            "{sql}: {}",
            String::from_utf8_lossy(&output)
        );
        run_with(&mut engine, &mut budget, "RESET ROLE");
    }
    // Plans that call none of the revoked functions still run; a strict
    // function with a constant NULL input folds without being called.
    for sql in [
        "SELECT a FROM filled ORDER BY a",
        "SELECT a FROM filled UNION ALL SELECT a FROM empty",
        "SELECT NULL::integer + 1",
        "SELECT greatest(a, 0) FROM filled",
    ] {
        let output = run_with(&mut engine, &mut budget, &format!("SET ROLE caller; {sql}"));
        assert!(
            error_sqlstates(&output).is_empty(),
            "{sql}: {}",
            String::from_utf8_lossy(&output)
        );
        run_with(&mut engine, &mut budget, "RESET ROLE");
    }
}

/// EXECUTE on user-defined routines is checked when a plan starts, like
/// built-ins, across every position a plan evaluates expressions; window
/// keys and row comparisons check each key's `=` function.
#[test]
fn plans_check_every_called_routine_before_running() {
    let (mut engine, mut budget) = test_engine();
    let output = run_with(
        &mut engine,
        &mut budget,
        "CREATE ROLE caller; CREATE TABLE t(a integer PRIMARY KEY, b text); \
         CREATE TABLE src(a integer, b text); INSERT INTO src VALUES (1, 'x'); \
         GRANT ALL ON t, src TO caller; \
         CREATE FUNCTION f(integer) RETURNS integer LANGUAGE sql AS 'SELECT $1'; \
         CREATE FUNCTION g(text) RETURNS boolean LANGUAGE plpgsql \
           AS 'BEGIN RETURN true; END'; \
         REVOKE EXECUTE ON FUNCTION f(integer), g(text) FROM PUBLIC",
    );
    assert!(
        error_sqlstates(&output).is_empty(),
        "{}",
        String::from_utf8_lossy(&output)
    );
    let denied = |engine: &mut _, budget: &mut _, sql: &str, function: &str| {
        let output = run_with(engine, budget, &format!("SET ROLE caller; {sql}"));
        assert_eq!(error_sqlstates(&output), ["42501"], "{sql}");
        assert!(
            String::from_utf8_lossy(&output)
                .contains(&format!("permission denied for function {function}")),
            "{sql}: {}",
            String::from_utf8_lossy(&output)
        );
        run_with(engine, budget, "RESET ROLE");
    };
    for (sql, function) in [
        ("SELECT f(a) FROM t", "f"),
        ("SELECT a FROM t WHERE g(b)", "g"),
        ("SELECT a FROM t ORDER BY f(a)", "f"),
        ("SELECT f(1) WHERE false", "f"),
        ("SELECT a FROM t WHERE a IN (SELECT f(a) FROM src)", "f"),
        ("INSERT INTO t SELECT f(a), b FROM t", "f"),
        ("INSERT INTO t VALUES (1, 'x') RETURNING f(a)", "f"),
        (
            "INSERT INTO t SELECT a, b FROM t ON CONFLICT (a) DO UPDATE SET a = f(1)",
            "f",
        ),
        ("UPDATE t SET b = 'q' RETURNING f(a)", "f"),
        ("DELETE FROM t RETURNING g(b)", "g"),
        (
            "MERGE INTO t USING src ON t.a = src.a WHEN MATCHED AND g(t.b) THEN DO NOTHING",
            "g",
        ),
        (
            "MERGE INTO t USING src ON t.a = src.a \
             WHEN NOT MATCHED THEN INSERT VALUES (f(1), 'z')",
            "f",
        ),
    ] {
        denied(&mut engine, &mut budget, sql, function);
    }
    run_with(
        &mut engine,
        &mut budget,
        "REVOKE EXECUTE ON FUNCTION texteq(text, text) FROM PUBLIC; \
         REVOKE EXECUTE ON FUNCTION int4eq(integer, integer) FROM PUBLIC",
    );
    for (sql, function) in [
        ("SELECT row_number() OVER (PARTITION BY b) FROM t", "texteq"),
        ("SELECT row_number() OVER (ORDER BY b) FROM src", "texteq"),
        ("SELECT (a, b) = (1, 'x') FROM t", "int4eq"),
        ("SELECT texteq('a', 'b')", "texteq"),
    ] {
        denied(&mut engine, &mut budget, sql, function);
    }
    // Planner-folded branches, ordering comparisons and unchecked plans run.
    for sql in [
        "SELECT 1 WHERE false AND g('x')",
        "SELECT CASE WHEN false THEN f(1) END",
        "SELECT (a, b) < (1, 'x') FROM t",
        "SELECT 1 FROM t ORDER BY b",
    ] {
        let output = run_with(&mut engine, &mut budget, &format!("SET ROLE caller; {sql}"));
        assert!(
            error_sqlstates(&output).is_empty(),
            "{sql}: {}",
            String::from_utf8_lossy(&output)
        );
        run_with(&mut engine, &mut budget, "RESET ROLE");
    }
    // Operator, cast and input functions run by name; initdb's ACL keeps
    // server administration functions from PUBLIC.
    let output = run_with(
        &mut engine,
        &mut budget,
        "SELECT texteq('a', 'a'), int4um(5), int4(2.5), int4in('7'), like('ab', 'a%'); \
         SELECT proacl FROM pg_proc WHERE oid = 'pg_reload_conf()'::regprocedure; \
         SET ROLE caller; SELECT pg_reload_conf()",
    );
    let text = String::from_utf8_lossy(&output);
    assert_eq!(error_sqlstates(&output), ["42501"], "{text}");
    for expected in [
        "-5",
        "{postgres=X/postgres}",
        "permission denied for function pg_reload_conf",
    ] {
        assert!(text.contains(expected), "{expected}: {text}");
    }
}

/// An empty `ARRAY[]` cast to a type takes that type without calling a cast
/// function, so changed built-in privileges never look for one.
#[test]
fn empty_array_casts_need_no_cast_function_after_privilege_changes() {
    let (mut engine, mut budget) = test_engine();
    run_with(
        &mut engine,
        &mut budget,
        "REVOKE EXECUTE ON FUNCTION texteq(text, text) FROM PUBLIC; \
         GRANT EXECUTE ON FUNCTION texteq(text, text) TO PUBLIC",
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT ARRAY[]::integer[], ARRAY[ARRAY[]::integer[], NULL::integer[]]"
        )),
        ["{}|{}"]
    );
}
