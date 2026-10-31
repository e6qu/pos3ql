use super::*;

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
        assert!(matches!(eval::constant_evaluation(expression,Some(&catalog)).unwrap(),
            Some(eval::CastEvaluation::Runtime)));
    }
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT dispatch_step(); SELECT last_value FROM dispatch_effects",true)),["17","17"]);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT setval('dispatch_target',dispatch_step()),currval('dispatch_effects')",
            true
        )),
        ["18|18"]
    );
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT last_value FROM dispatch_effects",true)),["18"]);
    for sql in ["SELECT hashtextextended(NULL::text,dispatch_step())", "SELECT setval(NULL::regclass,dispatch_step())",
        "SELECT setval(NULL::text::regclass,dispatch_step())"] {
        assert_eq!(data_rows(&session.success(&mut engine,sql,true)),["NULL"],"{sql}");
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT last_value FROM dispatch_effects",true)),["18"],"{sql}");
    }
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
