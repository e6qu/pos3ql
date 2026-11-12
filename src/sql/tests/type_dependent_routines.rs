use super::*;

/// A dropped type names itself and lists the routines whose signatures use
/// it, or reports them with `CASCADE`, as PostgreSQL's dependency report does.
#[test]
fn dropping_a_type_reports_its_routines() {
    let (mut engine, mut budget) = test_engine();
    let setup = run_with(
        &mut engine,
        &mut budget,
        "CREATE TYPE pair AS (a int); \
         CREATE FUNCTION takes(pair) RETURNS int LANGUAGE sql AS 'SELECT 1'; \
         CREATE FUNCTION gives(x int) RETURNS pair LANGUAGE sql AS 'SELECT ROW(1)::pair'",
    );
    assert!(error_sqlstates(&setup).is_empty());
    let refused = run_with(&mut engine, &mut budget, "DROP TYPE pair");
    let text = String::from_utf8_lossy(&refused);
    assert_eq!(error_sqlstates(&refused), ["2BP01"]);
    for expected in [
        "cannot drop type pair because other objects depend on it",
        "function takes(pair) depends on type pair\nfunction gives(integer) depends on type pair",
        "Use DROP ... CASCADE to drop the dependent objects too.",
    ] {
        assert!(text.contains(expected), "{expected}: {text}");
    }
    let dropped = run_with(&mut engine, &mut budget, "DROP TYPE pair CASCADE");
    let text = String::from_utf8_lossy(&dropped);
    assert!(error_sqlstates(&dropped).is_empty(), "{text}");
    for expected in [
        "drop cascades to 2 other objects",
        "drop cascades to function takes(pair)\ndrop cascades to function gives(integer)",
    ] {
        assert!(text.contains(expected), "{expected}: {text}");
    }
    let domain = run_with(
        &mut engine,
        &mut budget,
        "CREATE DOMAIN amount AS int; \
         CREATE FUNCTION spends(amount) RETURNS int LANGUAGE sql AS 'SELECT 1'; \
         DROP DOMAIN amount CASCADE",
    );
    let text = String::from_utf8_lossy(&domain);
    assert!(error_sqlstates(&domain).is_empty(), "{text}");
    assert!(
        text.contains("drop cascades to function spends(amount)"),
        "{text}"
    );
}
