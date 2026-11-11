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
