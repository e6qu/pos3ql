//! A qualified type name reports a missing schema (3F000) before a missing
//! type (42704), as PostgreSQL 18.6's `LookupTypeName` does, with the cursor
//! position and skip notices PostgreSQL gives.

use super::routine_identity_diagnostics::{diagnostics, error_fields};
use super::*;

fn first_error(
    engine: &mut Engine,
    budget: &mut Budget,
    sql: &str,
) -> (String, String, Option<String>) {
    let output = run_with(engine, budget, sql);
    let (code, message, _, position) = error_fields(&output)
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("{sql} succeeded"));
    (code, message, position)
}

#[test]
fn unknown_schema_precedes_unknown_type_at_the_type_name() {
    let (mut engine, mut budget) = test_engine();
    let schema = |position: Option<&str>| {
        (
            "3F000".to_owned(),
            "schema \"nope\" does not exist".to_owned(),
            position.map(str::to_owned),
        )
    };
    for (sql, expected) in [
        ("SELECT '1'::nope.int4", schema(Some("13"))),
        ("SELECT CAST(1 AS nope.t[])", schema(Some("18"))),
        ("SELECT '1'::postgres.nope.int4", schema(Some("13"))),
        (
            "CREATE TABLE unknown_schema (a nope.int4)",
            schema(Some("32")),
        ),
        (
            "CREATE DOMAIN unknown_schema AS nope.int4",
            schema(Some("33")),
        ),
        (
            "PREPARE unknown_schema(nope.int4) AS SELECT 1",
            schema(Some("24")),
        ),
        ("SELECT 'nope.int4'::regtype", schema(Some("8"))),
        ("DROP TYPE nope.t", schema(None)),
        (
            "SELECT '1'::pg_catalog.nope",
            (
                "42704".to_owned(),
                "type \"pg_catalog.nope\" does not exist".to_owned(),
                Some("13".to_owned()),
            ),
        ),
        (
            "SELECT 1::pg_catalog.integer",
            (
                "42704".to_owned(),
                "type \"pg_catalog.integer\" does not exist".to_owned(),
                Some("11".to_owned()),
            ),
        ),
        (
            "CREATE FUNCTION unknown_schema(int, public.nope[]) RETURNS int \
             LANGUAGE sql AS 'SELECT 1'",
            (
                "42704".to_owned(),
                "type public.nope[] does not exist".to_owned(),
                Some("37".to_owned()),
            ),
        ),
        (
            "SELECT '1'::a.b.int4",
            (
                "0A000".to_owned(),
                "cross-database references are not implemented: a.b.int4".to_owned(),
                None,
            ),
        ),
        (
            "SELECT '1'::a.b.c.int4",
            (
                "42601".to_owned(),
                "improper qualified name (too many dotted names): a.b.c.int4".to_owned(),
                None,
            ),
        ),
        (
            "SELECT to_regtype('a.b.c.int4')",
            (
                "42601".to_owned(),
                "improper qualified name (too many dotted names): a.b.c.int4".to_owned(),
                None,
            ),
        ),
        (
            "SELECT 'a.b.c.d'::regclass",
            (
                "42601".to_owned(),
                "improper relation name (too many dotted names): a.b.c.d".to_owned(),
                Some("8".to_owned()),
            ),
        ),
        (
            "SELECT 'a.b.c'::regclass",
            (
                "0A000".to_owned(),
                "cross-database references are not implemented: \"a.b.c\"".to_owned(),
                Some("8".to_owned()),
            ),
        ),
    ] {
        assert_eq!(
            first_error(&mut engine, &mut budget, sql),
            expected,
            "{sql}"
        );
    }
    let output = run_with(
        &mut engine,
        &mut budget,
        "SELECT '1'::postgres.pg_catalog.int4, to_regtype('nope.int4') IS NULL",
    );
    assert_eq!(data_rows(&output), ["1|t"]);
}

#[test]
fn drop_if_exists_names_the_missing_schema() {
    let (mut engine, mut budget) = test_engine();
    for sql in [
        "DROP TYPE IF EXISTS nope.t, public.nope",
        "DROP DOMAIN IF EXISTS nope.t, public.nope",
    ] {
        let output = run_with(&mut engine, &mut budget, sql);
        assert_eq!(
            diagnostics(&output),
            [
                (
                    'N',
                    "00000".to_owned(),
                    "schema \"nope\" does not exist, skipping".to_owned()
                ),
                (
                    'N',
                    "00000".to_owned(),
                    "type \"public.nope\" does not exist, skipping".to_owned()
                ),
            ],
            "{sql}"
        );
    }
}
