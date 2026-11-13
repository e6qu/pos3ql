use super::routine_identity_diagnostics::{Expected, NONE, check};
use super::*;

const BAD: Expected = (
    'N',
    "22023",
    "invalid value for parameter \"plpgsql.extra_warnings\": \"bad\"",
);
const BAD2: Expected = (
    'N',
    "22023",
    "invalid value for parameter \"plpgsql.extra_warnings\": \"bad2\"",
);
const LOAD: &str = "DO $$ BEGIN NULL; END $$";

/// Loading PL/pgSQL reapplies a session `SET` of a placeholder, then the
/// value of each nesting level that assigned it, whatever the value, and
/// warns for each value it rejects (PostgreSQL 18.6 `reapply_stacked_values`).
#[test]
fn plpgsql_load_warns_for_each_assigning_level() {
    let scenarios: &[&[(&str, &[Expected])]] = &[
        &[
            ("SET plpgsql.extra_warnings = 'bad'", NONE),
            ("BEGIN", NONE),
            ("SET plpgsql.extra_warnings = 'bad'", NONE),
            (LOAD, &[BAD, BAD]),
            ("COMMIT", NONE),
        ],
        &[("SET plpgsql.extra_warnings = 'bad'", NONE), (LOAD, &[BAD])],
        &[
            ("SET plpgsql.extra_warnings = 'bad'", NONE),
            ("BEGIN", NONE),
            ("SET plpgsql.extra_warnings = 'bad2'", NONE),
            ("SAVEPOINT a", NONE),
            ("SET plpgsql.extra_warnings = 'bad2'", NONE),
            ("SAVEPOINT b", NONE),
            (LOAD, &[BAD, BAD2, BAD2]),
            ("COMMIT", NONE),
        ],
        &[
            ("SET plpgsql.extra_warnings = 'bad'", NONE),
            ("BEGIN", NONE),
            ("SET LOCAL plpgsql.extra_warnings = 'bad'", NONE),
            (LOAD, &[BAD, BAD]),
            ("COMMIT", NONE),
        ],
        &[
            ("SET plpgsql.extra_warnings = 'bad'", NONE),
            ("RESET plpgsql.extra_warnings", NONE),
            ("BEGIN", NONE),
            ("SET plpgsql.extra_warnings = 'bad'", NONE),
            (LOAD, &[BAD]),
            ("COMMIT", NONE),
        ],
        &[
            ("BEGIN", NONE),
            ("SET plpgsql.extra_warnings = 'bad'", NONE),
            ("SAVEPOINT a", NONE),
            ("SET plpgsql.extra_warnings = 'bad2'", NONE),
            ("ROLLBACK TO a", NONE),
            (LOAD, &[BAD]),
            ("COMMIT", NONE),
        ],
    ];
    for scenario in scenarios {
        check(scenario);
    }
}

/// Each error's SQLSTATE, message, cursor position, internal position and
/// internal query, in wire order.
type ErrorFields = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn error_positions(bytes: &[u8]) -> Vec<ErrorFields> {
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
                field(b'P'),
                field(b'p'),
                field(b'q'),
            ));
        }
        offset += 1 + length;
    }
    found
}

fn session_engine(
    name: &str,
) -> (
    Engine,
    ConfiguredTransactionSession,
    crate::mem::budget::Budget,
) {
    let config = test_config(name);
    let mut budget = test_engine_budget(&config, 1 << 29);
    let engine = Engine::new(&config, &mut budget).unwrap();
    let session = ConfiguredTransactionSession::new(&config, &mut budget);
    (engine, session, budget)
}

fn one_error(error: (&str, &str, Option<usize>, Option<usize>, Option<&str>)) -> Vec<ErrorFields> {
    vec![(
        error.0.to_owned(),
        error.1.to_owned(),
        error.2.map(|position| position.to_string()),
        error.3.map(|position| position.to_string()),
        error.4.map(str::to_owned),
    )]
}

/// Positions from PostgreSQL 18.6: an operator's literal operand that
/// fails input, a field selection's base, a rule condition's missing `OLD`
/// or `NEW`, an argument without the common type, and a missing `$n`.
#[test]
fn analysis_errors_point_where_postgresql_does() {
    let (mut engine, mut session, _budget) = session_engine("procedural-compatibility-positions");
    session.success(
        &mut engine,
        "CREATE TABLE a(x int, y text); CREATE TABLE b(x int, y text); CREATE TABLE rt(n int)",
        true,
    );
    let at = |sql: &str, subject: &str| Some(sql.find(subject).unwrap() + 1);
    let cases: &[(&str, &str, &str, &str)] = &[
        (
            "SELECT 1 + 'x'",
            "22P02",
            "invalid input syntax for type integer: \"x\"",
            "'x'",
        ),
        (
            "SELECT 1 + 'x' FROM a",
            "22P02",
            "invalid input syntax for type integer: \"x\"",
            "'x'",
        ),
        (
            "SELECT (1).x",
            "42809",
            "column notation .x applied to type integer, which is not a composite type",
            "1)",
        ),
        (
            "SELECT (ROW(1, 2)).x",
            "42703",
            "could not identify column \"x\" in record data type",
            "ROW",
        ),
        (
            "SELECT (ROW(1, 'a')::a::record).nope",
            "42703",
            "column \"nope\" not found in data type a",
            "::a",
        ),
        (
            "SELECT COALESCE(a, b) FROM a, b",
            "42846",
            "COALESCE could not convert type b to a",
            "b)",
        ),
        (
            "SELECT COALESCE(1, true)",
            "42804",
            "COALESCE types integer and boolean cannot be matched",
            "true",
        ),
        (
            "SELECT CASE WHEN true THEN 1 ELSE true END",
            "42804",
            "CASE types boolean and integer cannot be matched",
            "1 ELSE",
        ),
        (
            "CREATE RULE r1 AS ON INSERT TO rt WHERE OLD.n > 0 DO NOTHING",
            "42P01",
            "invalid reference to FROM-clause entry for table \"old\"",
            "OLD",
        ),
    ];
    for (sql, state, message, subject) in cases {
        let output = session.execute(&mut engine, sql, true);
        assert_eq!(
            error_positions(&output),
            one_error((state, message, at(sql, subject), None, None)),
            "{sql}"
        );
    }
    let output = session.execute(
        &mut engine,
        "DO $$ DECLARE v int := 1; BEGIN v := v + 'x'; END $$",
        true,
    );
    assert_eq!(
        error_positions(&output),
        one_error((
            "22P02",
            "invalid input syntax for type integer: \"x\"",
            None,
            Some(10),
            Some("v := v + 'x'")
        ))
    );
    let output = session.execute(
        &mut engine,
        "DO $$ BEGIN CREATE RULE r3 AS ON INSERT TO rt WHERE old.n > 0 DO NOTHING; END $$",
        true,
    );
    assert_eq!(
        error_positions(&output),
        one_error((
            "42P01",
            "invalid reference to FROM-clause entry for table \"old\"",
            None,
            Some(41),
            Some("CREATE RULE r3 AS ON INSERT TO rt WHERE old.n > 0 DO NOTHING")
        ))
    );
}

/// `ALIAS FOR` shares the variable it names, `$n` is the `n`th parameter's
/// variable, and a `FOR` loop assigns each row's columns to a variable list.
#[test]
fn aliases_positional_parameters_and_loop_variable_lists() {
    let (mut engine, mut session, _budget) = session_engine("procedural-compatibility-aliases");
    session.success(
        &mut engine,
        "CREATE FUNCTION f1(p int) RETURNS text LANGUAGE plpgsql AS $$ BEGIN p := p + 1; RETURN $1::text || ' ' || p; END $$; \
         CREATE FUNCTION f3(int) RETURNS int LANGUAGE plpgsql AS $$ DECLARE a ALIAS FOR $1; BEGIN a := a + 10; RETURN $1; END $$; \
         CREATE FUNCTION f7(int) RETURNS int LANGUAGE plpgsql AS $$ <<o>> DECLARE x int := 3; BEGIN DECLARE a ALIAS FOR o.x; x int := 4; BEGIN RETURN a; END; END $$; \
         CREATE FUNCTION f17(OUT int, OUT text) LANGUAGE plpgsql AS $$ BEGIN $1 := 4; $2 := 'four'; END $$; \
         CREATE TABLE tt(n int); \
         CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE nr ALIAS FOR new; BEGIN nr.n := nr.n * 2; RETURN nr; END $$; \
         CREATE TRIGGER t1 BEFORE INSERT ON tt FOR EACH ROW EXECUTE FUNCTION tf(); \
         INSERT INTO tt VALUES (2)",
        true,
    );
    for (sql, expected) in [
        ("SELECT f1(1)", "2 2"),
        ("SELECT f3(1)", "11"),
        ("SELECT f7(1)", "3"),
        ("SELECT column1 || ' ' || column2 FROM f17()", "4 four"),
        ("SELECT n FROM tt", "4"),
    ] {
        assert_eq!(
            data_rows(&session.execute(&mut engine, sql, true)),
            [expected],
            "{sql}"
        );
    }
    use super::routine_identity_diagnostics::diagnostics;
    let notices = |session: &mut ConfiguredTransactionSession, engine: &mut Engine, sql: &str| {
        diagnostics(&session.execute(engine, sql, true))
            .into_iter()
            .map(|(_, state, message)| format!("{state} {message}"))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        notices(
            &mut session,
            &mut engine,
            "DO $$ DECLARE a int := 5; b text := 'q'; BEGIN \
             FOR a, b IN SELECT 1, 'x' UNION ALL SELECT 2, 'y' LOOP RAISE NOTICE '% %', a, b; END LOOP; \
             FOR a, b IN SELECT 1 WHERE false LOOP NULL; END LOOP; RAISE NOTICE '% %', a, b; END $$"
        ),
        ["00000 1 x", "00000 2 y", "00000 <NULL> <NULL>"]
    );
    assert_eq!(
        notices(
            &mut session,
            &mut engine,
            "DO $$ DECLARE a int; b int; BEGIN FOR a, b IN 1..3 LOOP NULL; END LOOP; END $$"
        ),
        ["42601 integer FOR loop must have only one target variable"]
    );
    assert_eq!(
        notices(
            &mut session,
            &mut engine,
            "DO $$ DECLARE a ALIAS FOR $1; BEGIN NULL; END $$"
        ),
        ["42704 variable \"$1\" does not exist"]
    );
}
