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
