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
