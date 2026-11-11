use super::*;

/// SQL-level PREPARE types every parameter as PostgreSQL 18.6 does: declared
/// types first, the rest from the statement, and `42P18` for a parameter
/// that is never referenced or only passed where any type is accepted.
#[test]
fn sql_prepare_records_inferred_parameter_types() {
    let config = test_config("prepared-parameter-types");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "PREPARE rounded AS SELECT round($1); \
         PREPARE echoed AS SELECT $1; \
         PREPARE added AS SELECT $1 + 1; \
         PREPARE mixed(int) AS SELECT $1, $2::text; \
         PREPARE measured AS SELECT length($1); \
         PREPARE absolute AS SELECT abs($1); \
         PREPARE joined AS SELECT $1 || 'x'; \
         PREPARE constant AS SELECT 1",
        true,
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT name, parameter_types::text, result_types::text \
               FROM pg_prepared_statements ORDER BY name",
            true
        )),
        [
            "absolute|{\"double precision\"}|{\"double precision\"}",
            "added|{integer}|{integer}",
            "constant|{}|{integer}",
            "echoed|{text}|{text}",
            "joined|{text}|{text}",
            "measured|{text}|{integer}",
            "mixed|{integer,text}|{integer,text}",
            "rounded|{\"double precision\"}|{\"double precision\"}",
        ]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "EXECUTE rounded(2.5); EXECUTE absolute(-2.5); EXECUTE echoed('x'); \
             EXECUTE constant(5)",
            true
        )),
        ["2", "2.5", "x", "1"]
    );
    for (sql, code, message) in [
        (
            "PREPARE typed AS SELECT pg_typeof($1)",
            "42P18",
            "could not determine data type of parameter $1",
        ),
        (
            "PREPARE skipped AS SELECT $2::int",
            "42P18",
            "could not determine data type of parameter $1",
        ),
        (
            "EXECUTE mixed(1)",
            "42601",
            "wrong number of parameters for prepared statement \"mixed\"",
        ),
        (
            "CREATE DOMAIN prepared_positive AS integer CHECK (VALUE > 0); \
             PREPARE checked AS SELECT $1::prepared_positive; EXECUTE checked(-1)",
            "23514",
            "prepared_positive",
        ),
        (
            "EXECUTE rounded('abc')",
            "22P02",
            "invalid input syntax for type double precision: \"abc\"",
        ),
    ] {
        let output = String::from_utf8_lossy(&session.execute(&mut engine, sql, true)).to_string();
        assert!(
            output.contains(code) && output.contains(message),
            "{sql}: {output}"
        );
    }
    let output =
        String::from_utf8_lossy(&session.execute(&mut engine, "EXECUTE rounded(1, 2)", true))
            .to_string();
    assert!(
        output.contains("Expected 1 parameters but got 2."),
        "{output}"
    );
}

/// PREPARE records user-defined parameter and result types exactly, and
/// EXECUTE converts each argument with PostgreSQL's assignment rules.
/// Expected rows and errors were captured from PostgreSQL 18.6.
#[test]
fn sql_prepare_records_user_defined_types_and_assigns_arguments() {
    let config = test_config("prepared-user-types");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE DOMAIN positive AS integer CHECK (VALUE > 0); \
         CREATE TYPE mood AS ENUM ('sad','happy'); \
         CREATE TYPE pair AS (a int, b text); \
         CREATE TABLE dt(p positive, m mood); \
         PREPARE a AS SELECT $1::positive; \
         PREPARE b AS SELECT $1::mood; \
         PREPARE c AS SELECT $1::pair; \
         PREPARE d(positive) AS SELECT $1; \
         PREPARE e(mood, pair) AS SELECT $1, $2; \
         PREPARE f AS INSERT INTO dt VALUES ($1, $2); \
         PREPARE g AS SELECT $1 = 'happy'::mood; \
         PREPARE h AS SELECT * FROM dt WHERE p = $1; \
         PREPARE i(positive[]) AS SELECT $1; \
         PREPARE j AS SELECT $1::mood[]; \
         PREPARE z(int) AS SELECT $1",
        true,
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT name, parameter_types::text, result_types::text \
               FROM pg_prepared_statements ORDER BY name",
            true
        )),
        [
            "a|{positive}|{positive}",
            "b|{mood}|{mood}",
            "c|{pair}|{pair}",
            "d|{positive}|{positive}",
            "e|{mood,pair}|{mood,pair}",
            "f|{positive,mood}|{}",
            "g|{mood}|{boolean}",
            "h|{integer}|{positive,mood}",
            "i|{positive[]}|{positive[]}",
            "j|{mood[]}|{mood[]}",
            "z|{integer}|{integer}",
        ]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "EXECUTE a(5); EXECUTE b('happy'); EXECUTE c('(1,x)'); EXECUTE d(3); \
             EXECUTE e('sad', '(2,y)'); EXECUTE e('sad', ROW(2,'y')); EXECUTE f(1, 'sad'); \
             EXECUTE g('happy'); EXECUTE h(1); EXECUTE i('{1,2}'); EXECUTE j('{sad}'); \
             EXECUTE d(2.7); EXECUTE z(5::bigint); EXECUTE z(2.5)",
            true
        )),
        [
            "5",
            "happy",
            "(1,x)",
            "3",
            "sad|(2,y)",
            "sad|(2,y)",
            "t",
            "1|sad",
            "{1,2}",
            "{sad}",
            "3",
            "5",
            "3",
        ]
    );
    for (sql, code, message) in [
        (
            "EXECUTE a(-1)",
            "23514",
            "value for domain positive violates check constraint \"positive_check\"",
        ),
        (
            "EXECUTE b('x')",
            "22P02",
            "invalid input value for enum mood: \"x\"",
        ),
        (
            "EXECUTE d(0)",
            "23514",
            "value for domain positive violates check constraint \"positive_check\"",
        ),
        (
            "EXECUTE f(-1, 'sad')",
            "23514",
            "value for domain positive violates check constraint \"positive_check\"",
        ),
        (
            "EXECUTE i('{1,-2}')",
            "23514",
            "value for domain positive violates check constraint \"positive_check\"",
        ),
        (
            "EXECUTE b('happy'::text)",
            "42804",
            "parameter $1 of type text cannot be coerced to the expected type mood",
        ),
        (
            "EXECUTE z('5'::text)",
            "42804",
            "parameter $1 of type text cannot be coerced to the expected type integer",
        ),
        (
            "EXECUTE z(true)",
            "42804",
            "parameter $1 of type boolean cannot be coerced to the expected type integer",
        ),
        (
            "PREPARE missing(no_such_type) AS SELECT $1",
            "42704",
            "type \"no_such_type\" does not exist",
        ),
    ] {
        let output = String::from_utf8_lossy(&session.execute(&mut engine, sql, true)).to_string();
        assert!(
            output.contains(code) && output.contains(message),
            "{sql}: {output}"
        );
        if code == "42804" {
            assert!(
                output.contains("You will need to rewrite or cast the expression."),
                "{sql}: {output}"
            );
        }
    }
    drop((session, engine));
    std::fs::remove_dir_all(config.data_dir).unwrap();
}
