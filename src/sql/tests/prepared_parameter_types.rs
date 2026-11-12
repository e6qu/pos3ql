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

fn rows(engine: &mut Engine, budget: &mut Budget, sql: &str) -> Vec<String> {
    let output = run_with(engine, budget, sql);
    assert!(
        error_sqlstates(&output).is_empty(),
        "{sql}: {}",
        String::from_utf8_lossy(&output)
    );
    data_rows(&output)
}

fn failure(engine: &mut Engine, budget: &mut Budget, sql: &str) -> (String, String) {
    let output = run_with(engine, budget, sql);
    let states = error_sqlstates(&output);
    assert_eq!(
        states.len(),
        1,
        "{sql}: {}",
        String::from_utf8_lossy(&output)
    );
    let text = String::from_utf8_lossy(&output);
    (states[0].to_string(), text.into_owned())
}

/// `PREPARE` parameter types keep their catalog identity: relation row types,
/// composites, domains, enums and their arrays coerce through the catalog at
/// `EXECUTE` with PostgreSQL's assignment rules.
#[test]
fn prepared_parameters_keep_catalog_type_identity() {
    let (mut engine, mut budget) = test_engine();
    rows(
        &mut engine,
        &mut budget,
        "CREATE TABLE t(a int, b text); \
         CREATE DOMAIN d AS int CHECK (VALUE > 0); \
         CREATE TYPE e AS ENUM ('x', 'y')",
    );
    // Prepared statements belong to one connection's simple-query batch here.
    let prepared = "PREPARE q(t) AS SELECT $1.a, $1.b; \
                    PREPARE q2(t, d, e, t[]) AS SELECT $1, $2, $3, $4; ";
    for (sql, expected) in [
        ("EXECUTE q(ROW(1, 'z'))", "1|z"),
        ("EXECUTE q('(2,w)')", "2|w"),
        ("EXECUTE q(ROW(1, 2))", "1|2"),
        (
            "EXECUTE q2('(1,x)', 3, 'y', ARRAY[ROW(1, 'a')::t])",
            "(1,x)|3|y|{\"(1,a)\"}",
        ),
    ] {
        assert_eq!(
            rows(&mut engine, &mut budget, &format!("{prepared}{sql}")),
            [expected],
            "{sql}"
        );
    }
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            &format!(
                "{prepared}SELECT name, parameter_types FROM pg_prepared_statements ORDER BY name"
            )
        ),
        ["q|{t}", "q2|{t,d,e,t[]}"]
    );
    let (state, text) = failure(&mut engine, &mut budget, &format!("{prepared}EXECUTE q(5)"));
    assert_eq!(state, "42804");
    assert!(
        text.contains("parameter $1 of type integer cannot be coerced to the expected type t"),
        "{text}"
    );
    assert_eq!(
        failure(
            &mut engine,
            &mut budget,
            &format!("{prepared}EXECUTE q(ROW(1))")
        )
        .0,
        "42846"
    );
    assert_eq!(
        failure(
            &mut engine,
            &mut budget,
            &format!("{prepared}EXECUTE q2('(1,x)', -3, 'y', NULL)")
        )
        .0,
        "23514"
    );
    let (state, text) = failure(
        &mut engine,
        &mut budget,
        &format!("{prepared}EXECUTE q(ROW(1, 'a'), 2)"),
    );
    assert_eq!(state, "42601");
    assert!(
        text.contains("wrong number of parameters for prepared statement \"q\""),
        "{text}"
    );
    let (state, text) = failure(&mut engine, &mut budget, "PREPARE bad(t) AS SELECT $1.zz");
    assert_eq!(state, "42703");
    assert!(
        text.contains("column \"zz\" not found in data type t"),
        "{text}"
    );
    assert_eq!(
        failure(&mut engine, &mut budget, "PREPARE bad(no_such) AS SELECT 1").0,
        "42704"
    );
}

/// `EXECUTE` arguments use assignment coercion, and parameters the
/// declaration omits are typed from their use, as PostgreSQL infers them.
#[test]
fn prepared_parameters_use_assignment_coercion_and_inference() {
    let (mut engine, mut budget) = test_engine();
    let prepared = "PREPARE i(int) AS SELECT $1; PREPARE r AS SELECT round($1); \
                    PREPARE mixed AS SELECT $1, $2 + 1, $3 || 'x'; ";
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            &format!("{prepared}EXECUTE i(1.5)")
        ),
        ["2"]
    );
    assert_eq!(
        failure(
            &mut engine,
            &mut budget,
            &format!("{prepared}EXECUTE i('1'::text)")
        )
        .0,
        "42804"
    );
    assert_eq!(
        failure(
            &mut engine,
            &mut budget,
            &format!("{prepared}EXECUTE i('x')")
        )
        .0,
        "22P02"
    );
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            &format!("{prepared}EXECUTE r(2.5)")
        ),
        ["2"]
    );
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            &format!("{prepared}EXECUTE mixed(1, 2, 3)")
        ),
        ["1|3|3x"]
    );
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            &format!(
                "{prepared}SELECT name, parameter_types FROM pg_prepared_statements ORDER BY name"
            )
        ),
        [
            "i|{integer}",
            "mixed|{text,integer,text}",
            "r|{\"double precision\"}"
        ]
    );
    assert_eq!(
        failure(
            &mut engine,
            &mut budget,
            &format!("{prepared}EXECUTE i((SELECT 4))")
        )
        .0,
        "0A000"
    );
    let (state, text) = failure(&mut engine, &mut budget, "PREPARE f AS SELECT ($1).a");
    assert_eq!(state, "42809");
    assert!(text.contains("applied to type unknown"), "{text}");
}

/// A client-unspecified parameter whose use determines no type is `unknown`
/// while analyzed and `text` afterwards.
#[test]
fn unspecified_parameter_fields_report_unknown() {
    let (engine, mut budget) = test_engine();
    let arena = Arena::new(&mut budget, "parameter inference", 1 << 18).unwrap();
    let transaction = TxnState::new(&mut budget, 64).unwrap();
    let error = engine
        .infer_param_types("SELECT ($1).a", &arena, &transaction, 1, &[], &[])
        .expect_err("a field of an unknown parameter is rejected");
    assert_eq!(error.sqlstate, "42809");
    assert_eq!(
        error.message.as_str(),
        "column notation .a applied to type unknown, which is not a composite type"
    );
    let inferred = engine
        .infer_param_types("SELECT $1", &arena, &transaction, 1, &[], &[])
        .unwrap();
    assert_eq!(inferred, [crate::sql::types::oid::TEXT]);
}

/// An untyped parameter takes its type from the built-in overload PostgreSQL
/// selects, or from the column it is compared with.
#[test]
fn untyped_parameters_take_call_and_comparison_types() {
    let config = test_config("prepared-parameter-uses");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "PREPARE typed_from_call AS SELECT abs($1), sqrt($2), upper($3), length($4); \
             EXECUTE typed_from_call(-1, 4, 'ab', 'abc'); \
             CREATE TABLE labelled (label text); \
             INSERT INTO labelled VALUES ('1'), ('3'); \
             PREPARE after_label AS SELECT label FROM labelled WHERE label > $1; \
             EXECUTE after_label(2)",
            true
        )),
        ["1|2|AB|3", "3"]
    );
}
