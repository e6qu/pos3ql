use super::*;

#[test]
fn named_routine_rows_expose_fields_in_table_sources() {
    let config = test_config("named-routine-table-rows");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let sql =
        include_str!("../../../tests/external/differential_exact/53_named_routine_table_rows.sql");
    assert_eq!(
        data_rows(&session.success(&mut engine, sql, true)),
        [
            "7|one|1",
            "NULL|NULL|2",
            "8|two|1",
            "NULL|NULL|2",
            "9|sql",
            "10|pl|1",
            "NULL|NULL|2",
            "1",
            "7|one|20|1",
            "NULL|NULL|21|2",
            "NULL|NULL|22|3",
            "1|7|one",
            "1|NULL|NULL",
            "2|7|one",
            "2|NULL|NULL",
            "(7,one)",
            "NULL",
        ]
    );
}

#[test]
fn retained_routine_inputs_preserve_argument_effects_across_pending_calls() {
    let config = test_config("retained-routine-inputs");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let sql =
        include_str!("../../../tests/external/differential_exact/50_retained_routine_inputs.sql");
    let output = session.success(&mut engine, sql, true);
    assert_eq!(
        data_rows(&output),
        [
            &["0", "1", "1", "1"][..],
            &["0", "8", "1", "7", "8"],
            &["0", "8", "1", "7", "8"],
            &["0", "8", "1", "7", "8"],
            &["0", "8", "1", "7", "8"],
            &["0", "17", "1", "7", "9", "17"],
            &["0", "8", "1", "7", "8"],
            &["0", "1", "2", "1", "1"],
            &["0", "1", "2", "1", "1"],
            &["0", "1", "1", "1"],
            &["0", "1/7", "1", "7"],
            &["0", "8/value", "1", "7"],
            &["0", "1", "2", "3", "1", "3"],
            &["0", "1", "2", "3", "1", "3"],
            &["0", "108", "0", "7", "108"],
            &["11", "7", "11"],
            &["5", "6", "integer", "integer"],
        ]
        .concat()
    );
    let mut descriptions = Vec::new();
    let mut remaining = output.as_slice();
    while !remaining.is_empty() {
        let length = i32::from_be_bytes(remaining[1..5].try_into().unwrap()) as usize;
        if remaining[0] == b'T' {
            descriptions.extend(row_description_type_oids(remaining));
        }
        remaining = &remaining[1 + length..];
    }
    assert_eq!(&descriptions[descriptions.len() - 2..], &[23, 2206]);
}

#[test]
fn retained_routine_targets_survive_argument_search_path_changes() {
    let config = test_config("retained-routine-targets");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let sql =
        include_str!("../../../tests/external/differential_exact/51_retained_routine_targets.sql");
    assert_eq!(
        data_rows(&session.success(&mut engine, sql, true)),
        ["11", "11", "22", "11", "22", "11", "11"]
    );
}

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

#[test]
fn routine_defaults_retain_creation_bindings_and_recover_without_a_cache() {
    let mut config = test_config("routine-default-creation-bindings");
    config.max_sequences = 6;
    config.max_routines = 16;
    config.object_store_on = true;
    config.object_store_sim = true;
    config.wal_upload = false;
    config.wal_upload_sync = false;
    config.object_store_bucket = format!("routine-default-bindings-{}", std::process::id());
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    let fixture =
        include_str!("../../../tests/external/differential_exact/45_routine_default_bindings.sql");
    let (exercise, remainder) = fixture.split_once("-- dependency errors").unwrap();
    let (dependency_errors, cleanup) = remainder.split_once("-- cleanup").unwrap();
    let mut budget = test_engine_budget(&config, 1 << 30);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    assert_eq!(
        data_rows(&session.success(&mut engine, exercise, true)),
        [
            "41|f",
            "41|42|7|901|301",
            "43",
            "44",
            "45|46|7|701|302",
            "47:702",
            "703",
            "48",
            "801|f",
            "901",
            "1|2",
            "100|100"
        ]
    );
    for sql in dependency_errors
        .lines()
        .filter(|line| !line.trim().is_empty())
    {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            ["2BP01"],
            "{sql}"
        );
    }
    assert!(engine.checkpoint().unwrap());
    drop(session);
    drop(engine);
    std::fs::remove_dir_all(&config.data_dir).unwrap();

    let mut cold_budget = test_engine_budget(&config, 1 << 30);
    let mut cold = Engine::new(&config, &mut cold_budget).unwrap();
    let mut cold_session = ConfiguredTransactionSession::new(&config, &mut cold_budget);
    // The object-store simulator allocates fixture keys during empty-cache reads.
    assert_eq!(
        data_rows(&cold_session.success(
            &mut cold,
            "SELECT public.default_scalar(),public.default_helper(),public.default_quoted()",
            false
        )),
        ["49|7|303"]
    );
    cold_session.success(&mut cold, cleanup, false);
    drop(cold_session);
    drop(cold);
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    std::fs::remove_dir_all(&config.data_dir).unwrap();
}

#[test]
fn routine_default_dependency_exhaustion_precedes_creation_and_effects() {
    let mut config = test_config("routine-default-dependency-capacity");
    config.max_stored_query_dependencies_per_object = 1;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE SEQUENCE default_limit_one; CREATE SEQUENCE default_limit_two",
        true,
    );
    assert_eq!(error_sqlstates(&session.execute(&mut engine,
        "CREATE FUNCTION default_limit(left_value bigint DEFAULT nextval('default_limit_one'),right_value bigint DEFAULT nextval('default_limit_two')) RETURNS bigint LANGUAGE sql AS 'SELECT $1+$2'", true)), ["54000"]);
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT to_regprocedure('default_limit(bigint,bigint)') IS NULL; SELECT is_called FROM default_limit_one; SELECT is_called FROM default_limit_two", true)), ["t", "f", "f"]);
}

#[test]
fn procedure_inputs_bind_before_effects_and_run_in_declaration_order() {
    let config = test_config("procedure-input-effects");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine,
        "CREATE SEQUENCE procedure_effects START 10; \
         CREATE TABLE procedure_audit(value bigint); \
         CREATE PROCEDURE procedure_pair(left_value bigint DEFAULT nextval('procedure_effects'),right_value bigint DEFAULT nextval('procedure_effects')) \
         LANGUAGE sql AS 'INSERT INTO procedure_audit VALUES ($1*100+$2)'", true);
    assert_eq!(data_rows(&session.success(&mut engine,
        "CALL procedure_pair(right_value=>nextval('procedure_effects')); \
         CALL procedure_pair(right_value=>nextval('procedure_effects'),left_value=>nextval('procedure_effects')); \
         SELECT value FROM procedure_audit ORDER BY value; SELECT last_value FROM procedure_effects", true)), ["1011", "1213", "13"]);
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "CALL procedure_missing(nextval('procedure_effects'))",
            true
        )),
        ["42883"]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT last_value FROM procedure_effects",
            true
        )),
        ["13"]
    );
}

#[test]
fn routine_default_calls_keep_their_captured_overload() {
    let mut config = test_config("routine-default-overloads");
    config.max_routines = 12;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            include_str!(
                "../../../tests/external/differential_exact/46_routine_default_overloads.sql"
            ),
            true
        )),
        ["11|1144", "22|11|1144", "11|1144", "11", "11"]
    );
}

#[test]
fn routine_inputs_share_conversions_order_and_strictness_in_every_context() {
    let mut config = test_config("routine-input-contexts");
    config.max_routines = 8;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let fixture =
        include_str!("../../../tests/external/differential_exact/47_routine_input_contexts.sql");
    let (setup, rest) = fixture.split_once("-- conversion errors").unwrap();
    let (errors, finish) = rest.split_once("-- after errors").unwrap();
    assert_eq!(
        data_rows(&session.success(&mut engine, setup, true)),
        ["1011", "1213", "14"]
    );
    let errors: Vec<_> = errors
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(errors.len(), 7);
    for (sql, state) in errors.into_iter().zip([
        "22P02", "22012", "22P02", "22012", "22012", "22P02", "42883",
    ]) {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            [state],
            "{sql}"
        );
    }
    assert_eq!(
        data_rows(&session.success(&mut engine, finish, true)),
        [
            "15",
            "16",
            "1",
            "17",
            "2",
            "18",
            "19",
            "0",
            "19",
            "1025",
            "20",
            "0",
            "21",
            "7|22",
            "7|7",
            "22",
            "bigint",
            "23",
            "fillfactor|80",
            "flag|NULL",
            "0"
        ]
    );
}

#[test]
fn pending_routine_calls_keep_bound_types_and_structural_values() {
    let config = test_config("pending-routine-types");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let sql =
        include_str!("../../../tests/external/differential_exact/48_pending_routine_types.sql");
    let (calls, replacement) = sql.split_once("-- replacement timing").unwrap();
    let mut rows = data_rows(&session.success(&mut engine, calls, true));
    rows.extend(data_rows(&session.success(&mut engine, replacement, true)));
    assert_eq!(
        rows,
        [
            "7|9|{1,NULL,3}",
            "7|9|{1,NULL,3}",
            "1|sql",
            "2|pl",
            "7|8",
            "3",
            "3",
            "4",
            "4",
            "5",
            "6",
            "7|7",
            "{1,NULL,3}|7",
            "8|8",
            "{2,NULL,4}|8",
            "12|named",
            "13|fallback",
            "101",
            "6|2|4|2|0|1"
        ]
    );
}

#[test]
fn record_call_expansion_preserves_scalar_effects_and_shared_set_producers() {
    let config = test_config("record-call-expansion");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let sql =
        include_str!("../../../tests/external/differential_exact/49_record_call_expansion.sql");
    let (valid, _) = sql.split_once("-- invalid field").unwrap();
    assert_eq!(
        data_rows(&session.success(&mut engine, valid, true)),
        [
            "10|11",
            "11",
            "12|13",
            "13",
            "1|14|15",
            "2|16|17",
            "17",
            "18|19",
            "set|set",
            "untyped|untyped",
            "integer",
            "20|20",
            "21|21",
            "20",
            "(21,21)|(22,22)",
            "(22,22)|(23,23)",
            "22",
            "(23,23)|(24,24)",
            "(24,24)|(25,25)",
            "24",
            "25|25",
            "26|26",
            "27|27",
            "26",
            "27|28",
            "29|30",
            "31|32",
            "33|34",
            "34",
            "a|1",
            "NULL|2",
            "c|3",
            "x|1",
            "NULL|2",
            "z|3",
            "7|named",
            "8|default",
            "4|7",
            "5|7"
        ]
    );
    for sql in ["SELECT (ARRAY[1,2]).f1", "SELECT (ARRAY[1,2]).x"] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            ["42809"]
        );
    }
    session.success(
        &mut engine,
        "CREATE FUNCTION capacity_record(OUT x integer,OUT y integer) LANGUAGE sql AS 'SELECT 1,2'",
        true,
    );
    for (additional, state) in [(MAX_PROJ - 2, None), (MAX_PROJ - 1, Some("54011"))] {
        let mut sql = String::from("SELECT (capacity_record()).*");
        for _ in 0..additional {
            sql.push_str(",1");
        }
        match state {
            None => {
                let output = session.success(&mut engine, &sql, true);
                assert_eq!(data_rows(&output)[0].split('|').count(), MAX_PROJ);
            }
            Some(state) => assert_eq!(
                error_sqlstates(&session.execute(&mut engine, &sql, true)),
                [state]
            ),
        }
    }
}
