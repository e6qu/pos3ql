use super::*;

fn rows(engine: &mut Engine, budget: &mut Budget, sql: &str) -> Vec<String> {
    let output = run_with(engine, budget, sql);
    assert!(
        error_sqlstates(&output).is_empty(),
        "{sql}: {}",
        String::from_utf8_lossy(&output)
    );
    data_rows(&output)
}

/// `pg_cast` lists PostgreSQL's built-in casts, and casts, operator classes
/// and operator families are described as `getObjectDescription` does.
#[test]
fn casts_and_operator_classes_are_described_like_postgresql() {
    let (mut engine, mut budget) = test_engine();
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT count(*), min(oid), max(oid) FROM pg_cast"
        ),
        ["235|10000|10234"]
    );
    for (sql, expected) in [
        (
            "SELECT pg_describe_object('pg_cast'::regclass, 10030, 0), \
             pg_identify_object('pg_cast'::regclass, 10030, 0)",
            "cast from money to numeric|(cast,,,\"(pg_catalog.money AS numeric)\")",
        ),
        (
            "SELECT pg_identify_object('pg_cast'::regclass, 10220, 0)",
            "(cast,,,\"(json AS pg_catalog.jsonb)\")",
        ),
        (
            "SELECT pg_describe_object('pg_opclass'::regclass, 1978, 0), \
             pg_identify_object('pg_opclass'::regclass, 1978, 0)",
            "operator class int4_ops for access method btree|(\"operator class\",pg_catalog,\
             int4_ops,\"pg_catalog.int4_ops USING btree\")",
        ),
        (
            "SELECT pg_describe_object('pg_opfamily'::regclass, 1976, 0)",
            "operator family integer_ops for access method btree",
        ),
    ] {
        assert_eq!(rows(&mut engine, &mut budget, sql), [expected], "{sql}");
    }
    rows(
        &mut engine,
        &mut budget,
        "CREATE TYPE mood AS ENUM ('a'); \
         CREATE FUNCTION mood_text(mood) RETURNS text LANGUAGE sql AS 'SELECT $1::text'; \
         CREATE CAST (mood AS text) WITH FUNCTION mood_text(mood); \
         CREATE SCHEMA hidden; CREATE OPERATOR FAMILY hidden.fam USING btree",
    );
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT pg_describe_object(tableoid, oid, 0) FROM pg_cast \
             WHERE castsource = 'mood'::regtype; \
             SELECT pg_describe_object(refclassid, refobjid, 0) FROM pg_depend \
             WHERE classid = 'pg_cast'::regclass AND objid = \
             (SELECT oid FROM pg_cast WHERE castsource = 'mood'::regtype) ORDER BY 1; \
             SELECT pg_describe_object('pg_opfamily'::regclass, oid, 0) FROM pg_opfamily \
             WHERE opfname = 'fam'"
        ),
        [
            "cast from mood to text",
            "function mood_text(mood)",
            "type mood",
            "operator family hidden.fam for access method btree"
        ]
    );
}
