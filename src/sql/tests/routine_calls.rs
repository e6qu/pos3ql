use super::*;

#[test]
fn constant_catalog_identifier_errors_do_not_depend_on_input_rows() {
    let config = test_config("routine-constant-identifiers");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "CREATE TABLE dispatch_empty(id integer)", true);
    for (sql, state) in [
        (
            "SELECT 'dispatch_missing_relation'::regclass FROM dispatch_empty",
            "42P01",
        ),
        (
            "SELECT 'dispatch_missing_type'::regtype FROM dispatch_empty",
            "42704",
        ),
        (
            "SELECT 'dispatch_missing_function'::regproc FROM dispatch_empty",
            "42883",
        ),
        (
            "SELECT 'dispatch_missing_function()'::regprocedure FROM dispatch_empty",
            "42883",
        ),
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            [state],
            "{sql}"
        );
    }
}

#[test]
fn stored_strict_calls_skip_constant_nulls_without_skipping_reached_inputs() {
    let config = test_config("routine-strict-inputs");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE SEQUENCE dispatch_strict_effects START 10; \
        CREATE FUNCTION dispatch_strict(first_arg bigint,second_arg bigint) RETURNS bigint \
        LANGUAGE sql VOLATILE STRICT AS 'SELECT $2'; \
        CREATE FUNCTION dispatch_strict_default(first_arg bigint,second_arg bigint DEFAULT NULL) \
        RETURNS bigint LANGUAGE sql VOLATILE STRICT AS 'SELECT $1'; \
        CREATE FUNCTION dispatch_strict_variadic(marker bigint,VARIADIC values_ bigint[]) \
        RETURNS bigint LANGUAGE sql VOLATILE STRICT AS 'SELECT $2[2]'; \
        CREATE FUNCTION dispatch_strict_variadic_default(marker bigint,VARIADIC values_ bigint[] DEFAULT NULL) \
        RETURNS bigint LANGUAGE sql VOLATILE STRICT AS 'SELECT $2[2]'",
        true,
    );
    for sql in [
        "SELECT dispatch_strict(NULL::bigint,nextval('dispatch_strict_effects'))",
        "SELECT dispatch_strict(second_arg=>nextval('dispatch_strict_effects'),first_arg=>NULL::bigint)",
        "SELECT dispatch_strict_default(nextval('dispatch_strict_effects'))",
        "SELECT dispatch_strict_variadic_default(nextval('dispatch_strict_effects'))",
        "SELECT dispatch_strict_variadic(nextval('dispatch_strict_effects'),VARIADIC NULL::bigint[])",
    ] {
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            ["NULL"],
            "{sql}"
        );
    }
    for (sql, state) in [
        ("SELECT dispatch_strict(NULL::bigint,1/0)", "22012"),
        ("SELECT dispatch_strict(NULL::bigint,'bad')", "22P02"),
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            [state],
            "{sql}"
        );
    }
    assert_eq!(
        data_rows(&session.success(&mut engine,
            "SELECT last_value,is_called FROM dispatch_strict_effects; \
            SELECT dispatch_strict_variadic(0,NULL::bigint,nextval('dispatch_strict_effects')); \
            SELECT dispatch_strict_variadic(0,VARIADIC ARRAY[NULL::bigint,nextval('dispatch_strict_effects')]); \
            CREATE TABLE dispatch_strict_inputs(id integer,value bigint); \
            INSERT INTO dispatch_strict_inputs VALUES (1,NULL),(2,1); \
            SELECT dispatch_strict(value,nextval('dispatch_strict_effects')) FROM dispatch_strict_inputs ORDER BY id; \
            SELECT last_value,is_called FROM dispatch_strict_effects",true)),
        ["10|f", "10", "11", "NULL", "13", "13|t"]
    );
}

#[test]
fn routine_arguments_run_once_and_preserve_strict_null_effects() {
    let mut config = test_config("routine-argument-effects");
    config.max_sequences = 2;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE SEQUENCE dispatch_target START 1000; CREATE SEQUENCE dispatch_effects START 10",
        true,
    );
    for (sql, expected) in [
        (
            "SELECT setval('dispatch_target',nextval('dispatch_effects')),currval('dispatch_effects')",
            "10|10",
        ),
        (
            "SELECT setval('dispatch_target',nextval('dispatch_effects')+nextval('dispatch_effects')),currval('dispatch_effects')",
            "23|12",
        ),
        (
            "SELECT setval('dispatch_target',nextval('dispatch_effects'),nextval('dispatch_effects')>0),currval('dispatch_effects')",
            "13|14",
        ),
        (
            "SELECT setval('dispatch_target',NULL::bigint,nextval('dispatch_effects')>0)",
            "NULL",
        ),
        ("SELECT last_value FROM dispatch_effects", "14"),
    ] {
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            [expected],
            "{sql}"
        );
    }
    session.success(
        &mut engine,
        "CREATE TABLE dispatch_inputs(id integer,name regclass,called boolean); \
        INSERT INTO dispatch_inputs VALUES (1,NULL,true),(2,'dispatch_target',NULL)",
        true,
    );
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT setval(name,nextval('dispatch_effects'),called) FROM dispatch_inputs ORDER BY id; \
        SELECT last_value FROM dispatch_effects",true)),["NULL","NULL","16"]);
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "SELECT dispatch_missing(nextval('dispatch_effects'))",
            true
        )),
        ["42883"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "SELECT last_value FROM dispatch_effects", true)),
        ["16"]
    );
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "SELECT setval(NULL::regclass,1/0,true)",
            true
        )),
        ["22012"]
    );
    session.success(
        &mut engine,
        "CREATE FUNCTION dispatch_step() RETURNS bigint LANGUAGE sql VOLATILE \
        AS 'SELECT nextval(''dispatch_effects'')'",
        true,
    );
    {
        let catalog = query::storage_catalog(&engine.storage, &engine.work, 0);
        let expression = parser::parse_expression("dispatch_step()", &engine.work).unwrap();
        assert!(matches!(
            eval::constant_evaluation(expression, Some(&catalog)).unwrap(),
            Some(eval::ExpressionEvaluation::Runtime)
        ));
    }
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT dispatch_step(); SELECT last_value FROM dispatch_effects",
            true
        )),
        ["17", "17"]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT setval('dispatch_target',dispatch_step()),currval('dispatch_effects')",
            true
        )),
        ["18|18"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "SELECT last_value FROM dispatch_effects", true)),
        ["18"]
    );
    for sql in [
        "SELECT hashtextextended(NULL::text,dispatch_step())",
        "SELECT setval(NULL::regclass,dispatch_step())",
        "SELECT setval(NULL::text::regclass,dispatch_step())",
        "SELECT setval((CASE WHEN true THEN NULL::text ELSE 'dispatch_target' END)::regclass,dispatch_step())",
    ] {
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            ["NULL"],
            "{sql}"
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT last_value FROM dispatch_effects",
                true
            )),
            ["18"],
            "{sql}"
        );
    }
    let oversized = format!(
        "SELECT dispatch_missing({})",
        vec!["nextval('dispatch_effects')"; 24].join(",")
    );
    let rejected = session.execute(&mut engine, &oversized, true);
    assert_eq!(error_sqlstates(&rejected), ["54000"]);
    assert!(String::from_utf8_lossy(&rejected).contains("192 bytes"));
    assert_eq!(
        data_rows(&session.success(&mut engine, "SELECT last_value FROM dispatch_effects", true)),
        ["18"]
    );
    session.success(
        &mut engine,
        "SELECT set_config('application_name','',false); \
        CREATE FUNCTION dispatch_comparison(regclass,integer) RETURNS boolean LANGUAGE sql VOLATILE \
        AS 'SELECT set_config(''application_name'',current_setting(''application_name'')||''x'',false) IS NOT NULL'; \
        CREATE OPERATOR === (LEFTARG=regclass,RIGHTARG=integer,FUNCTION=dispatch_comparison); \
        CREATE OPERATOR + (LEFTARG=regclass,RIGHTARG=integer,FUNCTION=dispatch_comparison)",
        true,
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT 'dispatch_target'::regclass === 1; SELECT current_setting('application_name'); \
            SELECT 'dispatch_target'::regclass + 1; SELECT current_setting('application_name')",
            true
        )),
        ["t", "x", "t", "xx"]
    );
}

#[test]
fn routine_named_variadic_and_default_inputs_keep_the_callers_effect_context() {
    let config = test_config("routine-bound-inputs");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE SEQUENCE dispatch_effects START 10; \
        CREATE FUNCTION dispatch_pair(left_value bigint,right_value bigint) RETURNS bigint \
        LANGUAGE sql VOLATILE AS 'SELECT $1*100+$2'; \
        CREATE FUNCTION dispatch_default(value bigint DEFAULT nextval('dispatch_effects')) \
        RETURNS bigint LANGUAGE sql VOLATILE AS 'SELECT $1'; \
        CREATE FUNCTION dispatch_variadic(VARIADIC values_ bigint[]) RETURNS bigint \
        LANGUAGE sql VOLATILE AS 'SELECT $1[1]*100+$1[2]'; \
        CREATE FUNCTION dispatch_defaults(left_value bigint DEFAULT nextval('dispatch_effects'), \
        right_value bigint DEFAULT nextval('dispatch_effects')) RETURNS bigint \
        LANGUAGE sql VOLATILE AS 'SELECT $1*100+$2'",
        true,
    );
    for (sql, expected) in [
        (
            "SELECT dispatch_pair(right_value=>nextval('dispatch_effects'),left_value=>nextval('dispatch_effects')),currval('dispatch_effects')",
            "1011|11",
        ),
        (
            "SELECT dispatch_variadic(nextval('dispatch_effects'),nextval('dispatch_effects')),currval('dispatch_effects')",
            "1213|13",
        ),
        (
            "SELECT dispatch_default(),currval('dispatch_effects')",
            "14|14",
        ),
        (
            "SELECT dispatch_default(nextval('dispatch_effects')),currval('dispatch_effects')",
            "15|15",
        ),
        (
            "SELECT dispatch_defaults(right_value=>nextval('dispatch_effects')),currval('dispatch_effects')",
            "1617|17",
        ),
        (
            "SELECT dispatch_defaults(),currval('dispatch_effects')",
            "1819|19",
        ),
    ] {
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            [expected],
            "{sql}"
        );
    }
}
