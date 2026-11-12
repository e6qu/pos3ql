//! Pseudo-types identify routine signatures and are refused as routine
//! argument or result types with PostgreSQL 18.6's validator messages.

use super::routine_identity_diagnostics::diagnostics;
use super::*;

fn single(engine: &mut Engine, budget: &mut Budget, sql: &str) -> String {
    let output = run_with(engine, budget, sql);
    assert!(
        error_sqlstates(&output).is_empty(),
        "{sql}: {}",
        String::from_utf8_lossy(&output)
    );
    data_rows(&output).join("\n")
}

fn error(engine: &mut Engine, budget: &mut Budget, sql: &str) -> (String, String) {
    let output = run_with(engine, budget, sql);
    let (_, code, message) = diagnostics(&output)
        .into_iter()
        .find(|(tag, _, _)| *tag == 'E')
        .unwrap_or_else(|| panic!("{sql} succeeded"));
    (code, message)
}

#[test]
fn pseudo_types_name_builtin_routine_signatures() {
    let (mut engine, mut budget) = test_engine();
    assert_eq!(
        single(
            &mut engine,
            &mut budget,
            "SELECT 'int4in(cstring)'::regprocedure, 'count(\"any\")'::regprocedure::oid, \
             'pg_catalog.int4in(pg_catalog.cstring)'::regprocedure::oid"
        ),
        "int4in(cstring)|2147|42"
    );
    assert_eq!(
        single(
            &mut engine,
            &mut budget,
            "SELECT 'cstring'::regtype::oid, '\"any\"'::regtype, 'pg_catalog.trigger'::regtype, \
             '_cstring'::regtype, 'cstring[]'::regtype::oid, 'INTEGER [3]'::regtype, \
             'pg_class'::regtype::oid, 'timestamp(3) with time zone'::regtype"
        ),
        "2275|\"any\"|trigger|cstring[]|1263|integer[]|83|timestamp with time zone"
    );
    assert_eq!(
        single(
            &mut engine,
            &mut budget,
            "SELECT pg_identify_object('pg_proc'::regclass, 'json_in(cstring)'::regprocedure, 0), \
             pg_describe_object('pg_proc'::regclass, 'format(text,\"any\")'::regprocedure, 0), \
             pg_identify_object('pg_proc'::regclass, 'varchar(varchar,int,bool)'::regprocedure, 0)"
        ),
        "(function,pg_catalog,,\"pg_catalog.json_in(pg_catalog.cstring)\")|function format(text,\"any\")|(function,pg_catalog,,\"pg_catalog.\"\"varchar\"\"(character varying,integer,boolean)\")"
    );
    single(
        &mut engine,
        &mut budget,
        "CREATE ROLE pseudo_grantee; \
         GRANT EXECUTE ON FUNCTION int4in(cstring) TO pseudo_grantee; \
         GRANT EXECUTE ON FUNCTION format(text, \"any\") TO pseudo_grantee",
    );
    assert_eq!(
        single(
            &mut engine,
            &mut budget,
            "SELECT has_function_privilege('pseudo_grantee', 'int4in(cstring)', 'EXECUTE')"
        ),
        "t"
    );
    for (sql, expected) in [
        (
            "SELECT 'any'::regtype",
            ("42601", "syntax error at or near \"any\""),
        ),
        (
            "SELECT '\"CSTRING\"'::regtype",
            ("42704", "type \"CSTRING\" does not exist"),
        ),
        (
            "SELECT 'pg_catalog.Foo'::regtype",
            ("42704", "type \"pg_catalog.foo\" does not exist"),
        ),
        (
            "SELECT 'nosuch.foo'::regtype",
            ("3F000", "schema \"nosuch\" does not exist"),
        ),
        (
            "SELECT NULL::fdw_handler",
            ("0A000", "cannot accept a value of type fdw_handler"),
        ),
    ] {
        let (code, message) = error(&mut engine, &mut budget, sql);
        assert_eq!((code.as_str(), message.as_str()), expected, "{sql}");
    }
}

#[test]
fn routine_validators_refuse_pseudo_types_like_postgresql() {
    let (mut engine, mut budget) = test_engine();
    for (sql, expected) in [
        (
            "CREATE FUNCTION pf(cstring) RETURNS int LANGUAGE sql AS 'SELECT 1'",
            (
                "42P13",
                "SQL functions cannot have arguments of type cstring",
            ),
        ),
        (
            "CREATE FUNCTION pf(\"any\") RETURNS int LANGUAGE sql AS 'SELECT 1'",
            (
                "42P13",
                "SQL functions cannot have arguments of type \"any\"",
            ),
        ),
        (
            "CREATE FUNCTION pf(x any) RETURNS int LANGUAGE sql AS 'SELECT 1'",
            ("42601", "syntax error at or near \"any\""),
        ),
        (
            "CREATE FUNCTION pf(record) RETURNS int LANGUAGE sql AS 'SELECT 1'",
            (
                "42P13",
                "SQL functions cannot have arguments of type record",
            ),
        ),
        (
            "CREATE FUNCTION pf(cstring) RETURNS int LANGUAGE plpgsql AS 'BEGIN RETURN 1; END'",
            ("0A000", "PL/pgSQL functions cannot accept type cstring"),
        ),
        (
            "CREATE FUNCTION pf() RETURNS cstring LANGUAGE sql AS 'SELECT 1'",
            ("42P13", "SQL functions cannot return type cstring"),
        ),
        (
            "CREATE FUNCTION pf() RETURNS trigger LANGUAGE sql AS 'SELECT 1'",
            ("42P13", "SQL functions cannot return type trigger"),
        ),
        (
            "CREATE FUNCTION pf(int) RETURNS fdw_handler LANGUAGE plpgsql AS 'BEGIN RETURN 1; END'",
            ("0A000", "PL/pgSQL functions cannot return type fdw_handler"),
        ),
        (
            "CREATE FUNCTION pf() RETURNS internal LANGUAGE sql AS 'SELECT 1'",
            ("42P13", "unsafe use of pseudo-type \"internal\""),
        ),
        (
            "CREATE FUNCTION pf(OUT a cstring) LANGUAGE sql AS 'SELECT 1'",
            ("42P13", "SQL functions cannot return type cstring"),
        ),
        (
            "CREATE AGGREGATE pa(cstring) (sfunc = int4pl, stype = int)",
            ("42883", "function int4pl(integer, cstring) does not exist"),
        ),
    ] {
        let (code, message) = error(&mut engine, &mut budget, sql);
        assert_eq!((code.as_str(), message.as_str()), expected, "{sql}");
    }
}
