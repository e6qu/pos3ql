use super::*;

/// Notices and errors in wire order as (`N` or `E`, SQLSTATE, message).
pub(super) fn diagnostics(bytes: &[u8]) -> Vec<(char, String, String)> {
    let mut found = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let tag = bytes[offset];
        let length = u32::from_be_bytes(bytes[offset + 1..offset + 5].try_into().unwrap()) as usize;
        if matches!(tag, b'N' | b'E') {
            let payload = &bytes[offset + 5..offset + 1 + length];
            let field = |key: u8| {
                payload
                    .split(|byte| *byte == 0)
                    .find(|value| value.first() == Some(&key))
                    .map(|value| String::from_utf8(value[1..].to_vec()).unwrap())
                    .unwrap()
            };
            found.push((tag as char, field(b'C'), field(b'M')));
        }
        offset += 1 + length;
    }
    found
}

/// Each error's SQLSTATE, message, DETAIL and cursor position, in wire order.
pub(super) fn error_fields(bytes: &[u8]) -> Vec<(String, String, Option<String>, Option<String>)> {
    let mut found = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let tag = bytes[offset];
        let length = u32::from_be_bytes(bytes[offset + 1..offset + 5].try_into().unwrap()) as usize;
        if tag == b'E' {
            let payload = &bytes[offset + 5..offset + 1 + length];
            let field = |key: u8| {
                payload
                    .split(|byte| *byte == 0)
                    .find(|value| value.first() == Some(&key))
                    .map(|value| String::from_utf8(value[1..].to_vec()).unwrap())
            };
            found.push((
                field(b'C').unwrap(),
                field(b'M').unwrap(),
                field(b'D'),
                field(b'P'),
            ));
        }
        offset += 1 + length;
    }
    found
}

/// One expected diagnostic: (`N` or `E`, SQLSTATE, message).
pub(super) type Expected = (char, &'static str, &'static str);

/// Expected PostgreSQL 18.6 diagnostics, captured from a reference server.
pub(super) fn check(cases: &[(&str, &[Expected])]) {
    let config = test_config("routine-identity-diagnostics");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    for (sql, expected) in cases {
        let output = session.execute(&mut engine, sql, true);
        let actual = diagnostics(&output);
        let expected: Vec<_> = expected
            .iter()
            .map(|(tag, state, message)| (*tag, state.to_string(), message.to_string()))
            .collect();
        assert_eq!(actual, expected, "{sql}");
    }
}

pub(super) const NONE: &[Expected] = &[];

#[test]
fn drop_routine_if_exists_sends_postgresql_skip_notices() {
    check(&[
        (
            "DROP FUNCTION IF EXISTS f(integer)",
            &[(
                'N',
                "00000",
                "function f(pg_catalog.int4) does not exist, skipping",
            )],
        ),
        (
            "DROP FUNCTION IF EXISTS f",
            &[('N', "00000", "function f() does not exist, skipping")],
        ),
        (
            "DROP PROCEDURE IF EXISTS p(int)",
            &[(
                'N',
                "00000",
                "procedure p(pg_catalog.int4) does not exist, skipping",
            )],
        ),
        (
            "DROP ROUTINE IF EXISTS r",
            &[('N', "00000", "routine r() does not exist, skipping")],
        ),
        (
            "DROP AGGREGATE IF EXISTS a(*)",
            &[('N', "00000", "aggregate a() does not exist, skipping")],
        ),
        (
            "DROP AGGREGATE IF EXISTS a(int ORDER BY text)",
            &[(
                'N',
                "00000",
                "aggregate a(pg_catalog.int4,text) does not exist, skipping",
            )],
        ),
        (
            "DROP FUNCTION IF EXISTS f(int4, \"int4\", pg_catalog.int4, varchar(3), \
             character varying, char, \"char\", bpchar, timestamptz, \
             timestamp(3) with time zone, time, double precision, float(3), \
             decimal(3,1), boolean, bool, bit varying(3), interval day to second, json, \
             jsonb, int8[], int[3][4], integer array, text[], PG_CATALOG.Int4, \
             national char varying, nchar(3), setof smallint, anyelement)",
            &[(
                'N',
                "00000",
                "function f(int4,int4,pg_catalog.int4,pg_catalog.varchar,pg_catalog.varchar,\
                 pg_catalog.bpchar,char,bpchar,timestamptz,pg_catalog.timestamptz,\
                 pg_catalog.time,pg_catalog.float8,pg_catalog.float4,pg_catalog.numeric,\
                 pg_catalog.bool,bool,pg_catalog.varbit,pg_catalog.interval,pg_catalog.json,\
                 jsonb,int8[],pg_catalog.int4[],pg_catalog.int4[],text[],pg_catalog.int4,\
                 pg_catalog.varchar,pg_catalog.bpchar,pg_catalog.int2,anyelement) does not \
                 exist, skipping",
            )],
        ),
        (
            "DROP FUNCTION IF EXISTS public.\"F x\"(int)",
            &[(
                'N',
                "00000",
                "function public.F x(pg_catalog.int4) does not exist, skipping",
            )],
        ),
        (
            "DROP FUNCTION IF EXISTS nosch.f(nosuch)",
            &[('N', "00000", "schema \"nosch\" does not exist, skipping")],
        ),
        (
            "DROP FUNCTION IF EXISTS f(nosch.t)",
            &[('N', "00000", "schema \"nosch\" does not exist, skipping")],
        ),
        (
            "DROP FUNCTION IF EXISTS f(int, nosuch[])",
            &[('N', "00000", "type \"nosuch[]\" does not exist, skipping")],
        ),
        (
            "DROP FUNCTION IF EXISTS f(\"INT4\")",
            &[('N', "00000", "type \"INT4\" does not exist, skipping")],
        ),
        (
            "CREATE FUNCTION kept(integer) RETURNS integer LANGUAGE sql AS 'SELECT 1'",
            NONE,
        ),
        // Every target resolves before any is dropped, so a repeated target
        // is dropped once and reports nothing.
        (
            "DROP FUNCTION IF EXISTS missing(int), kept(int), kept(int), other",
            &[
                (
                    'N',
                    "00000",
                    "function missing(pg_catalog.int4) does not exist, skipping",
                ),
                ('N', "00000", "function other() does not exist, skipping"),
            ],
        ),
        (
            "DROP FUNCTION IF EXISTS kept(int)",
            &[(
                'N',
                "00000",
                "function kept(pg_catalog.int4) does not exist, skipping",
            )],
        ),
    ]);
}

#[test]
fn routine_identity_errors_quote_postgresql_signatures() {
    check(&[
        (
            "DROP FUNCTION f(integer)",
            &[('E', "42883", "function f(integer) does not exist")],
        ),
        (
            "DROP FUNCTION f(varchar(3), timestamptz, int[], numeric(10,2), \"char\", char(5))",
            &[(
                'E',
                "42883",
                "function f(character varying, timestamp with time zone, integer[], numeric, \
                 \"char\", character) does not exist",
            )],
        ),
        (
            "DROP FUNCTION f",
            &[('E', "42883", "could not find a function named \"f\"")],
        ),
        (
            "DROP FUNCTION f()",
            &[('E', "42883", "function f() does not exist")],
        ),
        (
            "DROP PROCEDURE p(int)",
            &[('E', "42883", "procedure p(integer) does not exist")],
        ),
        (
            "DROP PROCEDURE p",
            &[('E', "42883", "could not find a procedure named \"p\"")],
        ),
        (
            "DROP ROUTINE r(int)",
            &[('E', "42883", "function r(integer) does not exist")],
        ),
        (
            "DROP AGGREGATE a(int ORDER BY text)",
            &[('E', "42883", "aggregate a(integer, text) does not exist")],
        ),
        (
            "DROP AGGREGATE a(*)",
            &[('E', "42883", "aggregate a(*) does not exist")],
        ),
        (
            "DROP FUNCTION pg_catalog.f(int)",
            &[(
                'E',
                "42883",
                "function pg_catalog.f(integer) does not exist",
            )],
        ),
        (
            "DROP FUNCTION nosch.f(int)",
            &[('E', "3F000", "schema \"nosch\" does not exist")],
        ),
        // A missing argument type is reported before a missing routine schema.
        (
            "DROP FUNCTION nosch.f(nosuch)",
            &[('E', "42704", "type \"nosuch\" does not exist")],
        ),
        (
            "DROP FUNCTION f(nosch.t)",
            &[('E', "3F000", "schema \"nosch\" does not exist")],
        ),
        (
            "CREATE SCHEMA hidden; CREATE TYPE hidden.pair AS (a int); \
             CREATE TYPE public.\"Odd\" AS (a int)",
            NONE,
        ),
        (
            "DROP FUNCTION f(hidden.pair, \"Odd\", hidden.pair[])",
            &[(
                'E',
                "42883",
                "function f(hidden.pair, \"Odd\", hidden.pair[]) does not exist",
            )],
        ),
        (
            "DROP FUNCTION f(anyelement, anyarray)",
            &[(
                'E',
                "42883",
                "function f(anyelement, anyarray) does not exist",
            )],
        ),
        (
            "ALTER FUNCTION nf(int) OWNER TO postgres",
            &[('E', "42883", "function nf(integer) does not exist")],
        ),
        (
            "ALTER AGGREGATE na(*) RENAME TO x",
            &[('E', "42883", "aggregate na(*) does not exist")],
        ),
        (
            "COMMENT ON FUNCTION nf(int) IS 'x'",
            &[('E', "42883", "function nf(integer) does not exist")],
        ),
        (
            "GRANT EXECUTE ON PROCEDURE np(int) TO public",
            &[('E', "42883", "procedure np(integer) does not exist")],
        ),
        (
            "DROP FUNCTION f(text, text, text, text, text, text, text, text, text, text, text, \
             text, text, text, text, text, text, text, text, text, text, text, text, text, \
             text, text, text, text, text, text, text, text, text, text, text)",
            &[(
                'E',
                "54000",
                "function diagnostic exceeds SQL error message capacity (192 bytes)",
            )],
        ),
    ]);
}

#[test]
fn routine_identity_kind_ambiguity_and_path_follow_postgresql() {
    check(&[
        (
            "CREATE FUNCTION fx(int) RETURNS int LANGUAGE sql AS 'SELECT 1'; \
             CREATE PROCEDURE px(int) LANGUAGE sql AS 'SELECT 1'; \
             CREATE AGGREGATE ag(int) (sfunc = int4pl, stype = int)",
            NONE,
        ),
        (
            "DROP FUNCTION IF EXISTS px(int)",
            &[('E', "42809", "px(integer) is not a function")],
        ),
        (
            "DROP FUNCTION IF EXISTS px",
            &[('N', "00000", "function px() does not exist, skipping")],
        ),
        (
            "DROP PROCEDURE fx(int)",
            &[('E', "42809", "fx(integer) is not a procedure")],
        ),
        (
            "DROP AGGREGATE fx(int)",
            &[('E', "42809", "function fx(integer) is not an aggregate")],
        ),
        (
            "DROP FUNCTION IF EXISTS ag",
            &[('E', "42809", "\"ag\" is an aggregate function")],
        ),
        (
            "ALTER FUNCTION px(int) OWNER TO postgres",
            &[('E', "42809", "px(integer) is not a function")],
        ),
        (
            "GRANT EXECUTE ON FUNCTION px(int) TO public",
            &[('E', "42809", "px(integer) is not a function")],
        ),
        // ORDER BY placement does not select a different aggregate.
        ("ALTER AGGREGATE ag(ORDER BY int) RENAME TO ag2", NONE),
        ("DROP AGGREGATE ag2(ORDER BY int)", NONE),
        (
            "CREATE FUNCTION fx(text) RETURNS int LANGUAGE sql AS 'SELECT 2'",
            NONE,
        ),
        (
            "DROP FUNCTION IF EXISTS fx",
            &[('E', "42725", "function name \"fx\" is not unique")],
        ),
        (
            "DROP ROUTINE fx",
            &[('E', "42725", "routine name \"fx\" is not unique")],
        ),
        (
            "DROP PROCEDURE fx",
            &[('E', "42883", "could not find a procedure named \"fx\"")],
        ),
        (
            "DROP FUNCTION fx(int), nosuch(int)",
            &[('E', "42883", "function nosuch(integer) does not exist")],
        ),
        // The failed statement dropped nothing.
        ("DROP FUNCTION fx(int), fx(text); DROP PROCEDURE px", NONE),
        (
            "CREATE SCHEMA s1; CREATE SCHEMA s2; \
             CREATE FUNCTION s1.sh(int) RETURNS int LANGUAGE sql AS 'SELECT 1'; \
             CREATE FUNCTION s2.sh(int) RETURNS int LANGUAGE sql AS 'SELECT 2'; \
             CREATE FUNCTION s2.sh(text) RETURNS int LANGUAGE sql AS 'SELECT 3'; \
             SET search_path = s1, s2, public",
            NONE,
        ),
        (
            "DROP FUNCTION IF EXISTS sh",
            &[('E', "42725", "function name \"sh\" is not unique")],
        ),
        // s1.sh(int) hides s2.sh(int), so the bare name selects s1.sh.
        ("DROP FUNCTION sh(text); DROP FUNCTION sh", NONE),
        ("DROP FUNCTION sh(int)", NONE),
        (
            "DROP FUNCTION sh(int)",
            &[('E', "42883", "function sh(integer) does not exist")],
        ),
    ]);
}

#[test]
fn routine_identity_errors_carry_postgresql_hints() {
    let config = test_config("routine-identity-hints");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE FUNCTION twice(int) RETURNS int LANGUAGE sql AS 'SELECT 1'; \
         CREATE FUNCTION twice(text) RETURNS int LANGUAGE sql AS 'SELECT 2'; \
         CREATE AGGREGATE total(int) (sfunc = int4pl, stype = int)",
        true,
    );
    for (sql, hint) in [
        (
            "DROP ROUTINE twice",
            "Specify the argument list to select the routine unambiguously.",
        ),
        (
            "ALTER FUNCTION twice OWNER TO postgres",
            "Specify the argument list to select the function unambiguously.",
        ),
        (
            "DROP FUNCTION total(int)",
            "Use DROP AGGREGATE to drop aggregate functions.",
        ),
    ] {
        let output = session.execute(&mut engine, sql, true);
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains(&format!("H{hint}\0")), "{sql}: {text}");
    }
}

#[test]
fn built_in_routine_targets_follow_postgresql() {
    let required = |name| ('E', "2BP01", name);
    check(&[
        (
            "DROP FUNCTION IF EXISTS lower(text)",
            &[required(
                "cannot drop function lower(text) because it is required by the database system",
            )],
        ),
        (
            "DROP FUNCTION substr(text, int)",
            &[required(
                "cannot drop function substr(text,integer) because it is required by the \
                 database system",
            )],
        ),
        (
            "DROP FUNCTION pg_catalog.now",
            &[required(
                "cannot drop function now() because it is required by the database system",
            )],
        ),
        (
            "DROP AGGREGATE count(*)",
            &[required(
                "cannot drop function count() because it is required by the database system",
            )],
        ),
        // Every target is resolved before a built-in is refused.
        (
            "DROP FUNCTION lower(text), nosuch(int)",
            &[('E', "42883", "function nosuch(integer) does not exist")],
        ),
        (
            "DROP FUNCTION lower",
            &[('E', "42725", "function name \"lower\" is not unique")],
        ),
        (
            "DROP PROCEDURE lower(text)",
            &[('E', "42809", "lower(text) is not a procedure")],
        ),
        (
            "DROP AGGREGATE lower(text)",
            &[('E', "42809", "function lower(text) is not an aggregate")],
        ),
        (
            "DROP FUNCTION sum(int)",
            &[('E', "42809", "\"sum\" is an aggregate function")],
        ),
        // The implicit pg_catalog comes first on the path, so a same-named
        // user function is reached only by its schema.
        (
            "CREATE FUNCTION public.lower(text) RETURNS text LANGUAGE sql AS 'SELECT $1'",
            NONE,
        ),
        (
            "DROP FUNCTION lower(text)",
            &[required(
                "cannot drop function lower(text) because it is required by the database system",
            )],
        ),
        ("DROP FUNCTION public.lower(text)", NONE),
        ("ALTER FUNCTION lower(text) STABLE", NONE),
        (
            "ALTER FUNCTION lower(text) RENAME TO lowered",
            &[(
                'E',
                "0A000",
                "built-in function lower(text) cannot be changed",
            )],
        ),
        ("ALTER FUNCTION lower(text) CALLED ON NULL INPUT", NONE),
        ("ALTER FUNCTION lower(text) SET search_path = public", NONE),
        ("ALTER FUNCTION lower(text) RESET ALL", NONE),
    ]);
}
