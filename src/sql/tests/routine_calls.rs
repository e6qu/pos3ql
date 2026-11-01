use super::*;

#[test]
fn query_positions_do_not_repeat_completed_projection_effects() {
    let config = test_config("retained-query-positions");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    // Independent cases commit retired definitions within the eight routine slots.
    let sql =
        include_str!("../../../tests/external/differential_exact/55_retained_query_positions.sql");
    let output = session.success(&mut engine, sql, true);
    assert_eq!(
        data_rows(&output),
        [
            &["0", "1|1", "2|2", "3|3", "3", "1", "2", "3"][..],
            &["0", "20", "20", "20", "3", "0|3", "20|3"],
            &["0", "3|99", "3", "0|3", "99|3"],
            &["0", "1|7|2|8|3", "3", "7|2", "8|1"],
            &["query_second,public|12", "12", "22", "12", "22"],
            &["query_second,pg_catalog,public|2|2", "shadow"],
            &["2|2", "777"],
            &["0|101", "109", "101", "109"],
            &[
                "0", "2", "0", "7", "9", "11", "15", "0", "0", "2", "7", "9", "11", "15"
            ],
            &["0|7", "NULL", "7", "0|NULL", "7", "7"],
            &["0|7", "NULL", "7", "0|NULL", "7", "7"],
            &["9", "7", "9", "11"],
            &["2|2", "9"],
        ]
        .concat()
    );
    let mut remaining = output.as_slice();
    let mut descriptions = 0;
    let mut completions = 0;
    while !remaining.is_empty() {
        let length = i32::from_be_bytes(remaining[1..5].try_into().unwrap()) as usize;
        descriptions += usize::from(remaining[0] == b'T');
        completions += usize::from(remaining[0] == b'C');
        remaining = &remaining[length + 1..];
    }
    assert_eq!(descriptions, 53);
    assert_eq!(completions, 137);
}

#[test]
fn named_routine_arguments_preserve_quoted_identifier_case() {
    let config = test_config("quoted-routine-arguments");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE quoted_argument_source(value integer); \
         INSERT INTO quoted_argument_source VALUES(2); \
         CREATE FUNCTION quoted_argument_input(\"Value\" integer) RETURNS integer \
         LANGUAGE sql VOLATILE AS $$SELECT $1$$; \
         CREATE FUNCTION folded_argument_input(VALUE integer) RETURNS integer \
         LANGUAGE sql VOLATILE AS $$SELECT $1$$; \
         CREATE PROCEDURE quoted_argument_output(OUT \"Result\" integer,\"Value\" integer, \
         VARIADIC \"Members\" integer[]) LANGUAGE sql AS $$SELECT $1+cardinality($2)$$",
        true,
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT quoted_argument_input(\"Value\"=>value), \
             folded_argument_input(VALUE=>value) FROM quoted_argument_source; \
             CALL quoted_argument_output(\"Result\"=>NULL,\"Value\"=>9, \
             VARIADIC \"Members\"=>ARRAY[]::integer[])",
            true,
        )),
        ["2|2", "9"]
    );
    for sql in [
        "SELECT quoted_argument_input(value=>value) FROM quoted_argument_source",
        "SELECT quoted_argument_input(\"value\"=>value) FROM quoted_argument_source",
        "CALL quoted_argument_output(\"result\"=>NULL,\"Value\"=>9, \
         VARIADIC \"Members\"=>ARRAY[]::integer[])",
        "CALL quoted_argument_output(\"Result\"=>NULL,value=>9, \
         VARIADIC \"Members\"=>ARRAY[]::integer[])",
        "CALL quoted_argument_output(\"Result\"=>NULL,\"Value\"=>9, \
         VARIADIC members=>ARRAY[]::integer[])",
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            ["42883"],
            "{sql}"
        );
    }
    let actual = [
        types::oid::UNKNOWN,
        types::oid::INT4,
        ColType::Array(types::ArrElem::Int4).oid(),
    ];
    let expected = engine
        .storage
        .procedure_call_parameter_oids(
            "quoted_argument_output",
            &[Some("Result"), Some("Value"), Some("Members")],
            true,
            &actual,
            0,
        )
        .expect("quoted argument identities resolve protocol types");
    assert_eq!(
        &expected[..actual.len()],
        &[types::oid::INT4, types::oid::INT4, actual[2]]
    );
    assert!(
        engine
            .storage
            .procedure_call_parameter_oids(
                "quoted_argument_output",
                &[Some("result"), Some("Value"), Some("Members")],
                true,
                &actual,
                0,
            )
            .is_none()
    );
}

#[test]
fn retained_variadic_calls_require_explicit_empty_arrays_or_declared_defaults() {
    let config = test_config("retained-variadic-arity");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE retained_variadic_source(value integer); \
         INSERT INTO retained_variadic_source VALUES(1); \
         CREATE FUNCTION retained_variadic_required(VARIADIC members integer[]) \
         RETURNS integer LANGUAGE sql VOLATILE AS $$SELECT cardinality($1)$$; \
         CREATE FUNCTION retained_variadic_named_default(marker integer DEFAULT 7, \
         VARIADIC members integer[] DEFAULT ARRAY[]::integer[]) RETURNS integer \
         LANGUAGE sql VOLATILE AS $$SELECT $1+cardinality($2)$$",
        true,
    );
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "SELECT retained_variadic_required() FROM retained_variadic_source",
            true,
        )),
        ["42883"]
    );
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "SELECT retained_variadic_named_default(marker=>9) FROM retained_variadic_source",
            true,
        )),
        ["42883"]
    );
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "CREATE FUNCTION retained_variadic_invalid(marker integer DEFAULT 7, \
             VARIADIC members integer[]) RETURNS integer LANGUAGE sql \
             AS $$SELECT $1+cardinality($2)$$",
            true,
        )),
        ["42P13"]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT retained_variadic_required(VARIADIC ARRAY[]::integer[]) \
             FROM retained_variadic_source",
            true,
        )),
        ["0"]
    );
}

#[test]
fn variadic_procedures_preserve_output_positions_and_named_array_syntax() {
    let config = test_config("variadic-procedure-mapping");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE PROCEDURE variadic_output(OUT result integer,marker integer DEFAULT 7, \
         VARIADIC members integer[] DEFAULT ARRAY[]::integer[]) LANGUAGE sql \
         AS $$SELECT $1+cardinality($2)$$",
        true,
    );
    for sql in [
        "CALL variadic_output(result=>NULL)",
        "CALL variadic_output(result=>NULL,marker=>9)",
        "CALL variadic_output(result=>NULL,marker=>9,members=>ARRAY[]::integer[])",
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            ["42883"],
            "{sql}"
        );
    }
    for sql in [
        "CREATE PROCEDURE invalid_variadic_output(VARIADIC members integer[], \
         OUT result integer) LANGUAGE sql AS $$SELECT cardinality($1)$$",
        "CREATE PROCEDURE invalid_variadic_default_output(marker integer DEFAULT 7, \
         VARIADIC members integer[] DEFAULT ARRAY[]::integer[],OUT result integer) \
         LANGUAGE sql AS $$SELECT $1+cardinality($2)$$",
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            ["42P13"],
            "{sql}"
        );
    }
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "CALL variadic_output(result=>NULL,marker=>9,VARIADIC members=>ARRAY[]::integer[]); \
             CALL variadic_output(NULL); \
             CALL variadic_output(NULL,9,VARIADIC ARRAY[]::integer[]); \
             CALL variadic_output(NULL,9,4,5)",
            true,
        )),
        ["9", "7", "9", "11"]
    );
}

#[test]
fn retained_query_errors_rollback_completed_writes_and_session_changes() {
    let config = test_config("retained-query-rollback");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE query_rollback_source(value integer); \
         INSERT INTO query_rollback_source VALUES(1); \
         CREATE TABLE query_rollback_audit(value integer); \
         CREATE SEQUENCE query_rollback_numbers; \
         CREATE FUNCTION query_rollback_write(value integer) RETURNS integer \
         LANGUAGE sql VOLATILE AS \
         $$INSERT INTO query_rollback_audit VALUES($1); SELECT $1$$; \
         SELECT set_config('application_name','0',false)",
        true,
    );
    session.success(&mut engine, "BEGIN", true);
    let output = session.execute(
        &mut engine,
        "SELECT nextval('query_rollback_numbers'), \
         set_config('application_name','changed',false), \
         query_rollback_write(value), 1/(value-1) FROM query_rollback_source",
        true,
    );
    assert_eq!(error_sqlstates(&output), ["22012"]);
    session.success(&mut engine, "ROLLBACK", true);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT current_setting('application_name'); \
             SELECT count(*) FROM query_rollback_audit; \
             SELECT last_value,is_called FROM query_rollback_numbers; \
             SELECT query_rollback_write(value) FROM query_rollback_source; \
             SELECT value FROM query_rollback_audit",
            true,
        )),
        ["0", "0", "1|t", "1", "1"]
    );
}

#[test]
fn retained_query_call_state_is_reused_between_rows() {
    const ROWS: usize = 1024;
    let config = test_config("retained-query-bounded-rows");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let statement_bytes = crate::storage::RowCursor::position_bytes(ROWS, 0).unwrap() + (128 << 10);
    let mut session =
        ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, statement_bytes);
    session.success(
        &mut engine,
        "CREATE TABLE query_bounded_source(value integer); \
         CREATE TABLE query_bounded_audit(value integer); \
         CREATE FUNCTION query_bounded_write(value integer) RETURNS integer \
         LANGUAGE sql VOLATILE AS \
         $$INSERT INTO query_bounded_audit VALUES($1); SELECT $1$$",
        true,
    );
    for value in 1..=ROWS {
        session.success(
            &mut engine,
            &format!("INSERT INTO query_bounded_source VALUES({value})"),
            true,
        );
    }
    session.success(
        &mut engine,
        "SELECT set_config('application_name','0',false)",
        true,
    );
    let output = session.success(
        &mut engine,
        "SELECT set_config('application_name', \
         (current_setting('application_name')::integer+1)::text,false), \
         query_bounded_write(value) FROM query_bounded_source",
        true,
    );
    let rows = data_rows(&output);
    assert_eq!(rows.len(), ROWS);
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row.split('|').next().unwrap(), (index + 1).to_string());
    }
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT current_setting('application_name'); \
             SELECT count(*),count(DISTINCT value),sum(value) FROM query_bounded_audit",
            true,
        )),
        ["1024", "1024|1024|524800"]
    );
}

#[test]
fn intrinsic_handoffs_retain_completed_builtin_inputs() {
    let config = test_config("retained-intrinsic-inputs");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let sql =
        include_str!("../../../tests/external/differential_exact/54_retained_intrinsic_inputs.sql");
    assert_eq!(
        data_rows(&session.success(&mut engine, sql, true)),
        ["0", "0.", "0.", "0", "1", "0."]
    );
}

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
            "11|out",
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
            "4|new",
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

#[test]
fn retained_query_handoffs_count_one_scan_and_each_source_row_once() {
    let config = test_config("retained-query-scan-counts");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE query_scan_source(value integer); \
         INSERT INTO query_scan_source VALUES(1),(2),(3); \
         CREATE TABLE query_scan_audit(value integer); \
         CREATE FUNCTION query_scan_write(value integer) RETURNS integer \
         LANGUAGE sql VOLATILE AS \
         $$INSERT INTO query_scan_audit VALUES($1); SELECT $1$$",
        true,
    );
    session.success(&mut engine, "BEGIN", true);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT query_scan_write(value) FROM query_scan_source; \
             SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables \
             WHERE relid='query_scan_source'::regclass",
            true,
        )),
        ["1", "2", "3", "1|3"]
    );
    session.success(&mut engine, "ROLLBACK", true);
    assert_eq!(
        data_rows(&session.success(&mut engine, "SELECT count(*) FROM query_scan_audit", true,)),
        ["0"]
    );
}

#[test]
fn qualified_builtin_types_survive_mutable_function_handoffs() {
    let config = test_config("qualified-builtin-handoff-types");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE qualified_builtin_source(value integer); \
         INSERT INTO qualified_builtin_source VALUES(2); \
         CREATE TABLE qualified_builtin_audit(value integer); \
         CREATE FUNCTION qualified_builtin_write(value integer) RETURNS integer \
         LANGUAGE sql VOLATILE AS \
         $$INSERT INTO qualified_builtin_audit VALUES($1); SELECT $1$$",
        true,
    );
    for prefix in ["", "pg_catalog."] {
        let sql = format!(
            "SELECT {prefix}abs(-7.5)+1.0, \
             {prefix}ln({prefix}abs(-7.5)+1.0), \
             {prefix}hashfloat8extended(0::float8,23)+1, \
             {prefix}hashtext(NULL::text), \
             {prefix}hashtextextended(NULL::text,0), \
             qualified_builtin_write(value) FROM qualified_builtin_source"
        );
        let output = session.success(&mut engine, &sql, true);
        assert_eq!(
            row_description_type_oids(&output),
            [1700, 1700, 20, 23, 20, 23]
        );
        let rows = data_rows(&output);
        assert_eq!(rows.len(), 1);
        let values = rows[0].split('|').collect::<Vec<_>>();
        assert_eq!(values[0], "8.5");
        assert!((values[1].parse::<f64>().unwrap() - 8.5f64.ln()).abs() < 1e-12);
        assert_eq!(&values[2..], &["24", "NULL", "NULL", "2"]);
    }
    session.success(
        &mut engine,
        "CREATE SCHEMA qualified_builtin_user; \
         CREATE FUNCTION qualified_builtin_user.abs(value numeric) RETURNS text \
         LANGUAGE sql IMMUTABLE AS $$SELECT 'custom'::text$$; \
         CREATE FUNCTION qualified_builtin_user.hashtextextended(value text,seed bigint) \
         RETURNS numeric LANGUAGE sql IMMUTABLE AS $$SELECT 123.5$$",
        true,
    );
    let output = session.success(
        &mut engine,
        "SELECT qualified_builtin_user.abs(-7.5), \
         qualified_builtin_user.hashtextextended('label',0)+1.0, \
         qualified_builtin_write(value) FROM qualified_builtin_source",
        true,
    );
    assert_eq!(row_description_type_oids(&output), [25, 1700, 23]);
    assert_eq!(data_rows(&output), ["custom|124.5|2"]);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT count(*) FROM qualified_builtin_audit",
            true,
        )),
        ["3"]
    );
}

#[test]
fn decoded_guc_values_keep_their_spelling_and_resolve_search_path_names() {
    let config = test_config("decoded-guc-values");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let sql = include_str!(
        "../../../tests/external/differential_exact/56_search_path_value_assignment.sql"
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, sql, true)),
        [
            "GUC_RAW,public",
            "GUC_RAW,public|7",
            "  \"Guc Space\" , public  ",
            "8",
            "\x0bpublic\x0b",
            "public",
            "public\x0b,\x0bpg_catalog",
            "public",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "\"ééééééééééééééééééééééééééééééééa\"",
            "\"éééééééééééééééééééééééééééééééééééééééééééééééééé\"",
            "guc_raw, public",
            "\"guc_raw,public\"",
            "'public'",
            "  abc  ",
            "'abc'",
            "DEFAULT",
            "DEFAULT",
            "DEFAULT",
            "\"$user\", public",
            "GUC_RAW,public",
            "public",
            "GUC_RAW,public|public",
            "GUC_RAW,public",
            "  \"Guc Space\" , public  ",
            "GUC_RAW,public",
            "public",
        ]
    );
    session.success(
        &mut engine,
        "SELECT set_config('search_path','GUC_RAW,public',false)",
        true,
    );
    for value in [
        "foo bar",
        "public\x0bpg_catalog",
        "public,\x0b",
        "public,,pg_catalog",
        "public,",
        "\"unclosed",
        "\"public\"tail",
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(
                &mut engine,
                &format!("SELECT set_config('search_path','{value}',false)"),
                true
            )),
            ["22023"]
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, "SELECT current_setting('search_path')", true)),
            ["GUC_RAW,public"]
        );
    }
}

#[test]
fn sql_setting_strings_are_decoded_once_before_guc_assignment() {
    let config = test_config("decoded-sql-setting-values");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let sql =
        include_str!("../../../tests/external/differential_exact/57_decoded_sql_settings.sql");
    assert_eq!(
        data_rows(&session.success(&mut engine, sql, true)),
        [
            "a'b",
            "dollar'quote",
            "doubled'quote",
            "a\\x09b",
            "DEFAULT",
            "1",
            "\"Case\tName\", public",
            "\"quote'name\", \"dollar,name\", \"Mixed Case\"",
            "ISO, DMY",
            "function'value|outside",
            "changed'value|outside",
            "outside|t",
            "outside|t",
            "DEFAULT",
            "application_name=role'value",
            "application_name=database'value",
            "UTC",
            "<+02>-02",
            "<+02>-02",
            "<+02>-02",
            "<-02>+02",
            "<+02:30>-02:30",
            "9000",
            "+02:00",
            "-7200",
            "<+02:30>-02:30",
        ]
    );
    for sql in [
        "SET application_name TO one,two",
        "SET nonexistent_parameter TO one,two",
        "SET server_version TO one,two",
        "SET TIME ZONE 'DEFAULT'",
        "SET TIME ZONE 'LOCAL'",
        "SET TIME ZONE $$DEFAULT$$",
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            ["22023"],
            "{sql}"
        );
    }
}

#[test]
fn search_path_sql_capacity_errors_preserve_the_selected_value() {
    let config = test_config("search-path-sql-capacity");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    for (name, expected) in [
        ("a".repeat(63), "a".repeat(63)),
        (
            format!("{}a", "é".repeat(31)),
            format!("\"{}a\"", "é".repeat(31)),
        ),
    ] {
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                &format!("SET search_path TO {name}; SHOW search_path"),
                true,
            )),
            [expected]
        );
    }
    session.success(
        &mut engine,
        "SELECT set_config('search_path','GUC_RAW,public',false)",
        true,
    );
    for name in [
        "a".repeat(64),
        "a".repeat(65),
        "é".repeat(32),
        format!("{}a", "é".repeat(32)),
    ] {
        for sql in [
            format!("SET search_path TO {name}"),
            format!("SET search_path TO \"{name}\""),
        ] {
            assert_eq!(
                error_sqlstates(&session.execute(&mut engine, &sql, true)),
                ["42622"]
            );
            assert_eq!(
                data_rows(&session.success(&mut engine, "SHOW search_path", true)),
                ["GUC_RAW,public"]
            );
        }
    }
    for value in ["a".repeat(129), "é".repeat(64), "é".repeat(65)] {
        assert_eq!(
            error_sqlstates(&session.execute(
                &mut engine,
                &format!("SET search_path TO '{value}'"),
                true,
            )),
            ["22023"]
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, "SHOW search_path", true)),
            ["GUC_RAW,public"]
        );
    }
}

#[test]
fn saved_guc_values_preserve_raw_spelling_without_runtime_allocations() {
    let guc = GucState::new();
    let raw = "  \"Mixed Case\" , public  ";
    let configs = [crate::storage::RoutineConfig {
        name: crate::storage::SqlName::parse("search_path").unwrap(),
        value: crate::util::StackStr::from_str(raw),
    }];
    crate::mem::guard::forbid_alloc(|| {
        guc.set_connection_default(
            "search_path",
            "GUC_RAW,public",
            guc::ConnectionDefaultSource::Role,
        )
        .unwrap();
        assert_eq!(
            guc.get_owned("search_path").unwrap().as_str(),
            "GUC_RAW,public"
        );
        guc.begin_transaction();
        guc.set_config("search_path", Some("public"), false)
            .unwrap();
        guc.commit_transaction();
        guc.begin_transaction();
        guc.set_config("search_path", None, false).unwrap();
        guc.commit_transaction();
        assert_eq!(
            guc.get_owned("search_path").unwrap().as_str(),
            "GUC_RAW,public"
        );
        {
            let _scope = guc.enter_routine_configs(&configs).unwrap();
            assert_eq!(guc.get_owned("search_path").unwrap().as_str(), raw);
        }
        assert_eq!(
            guc.get_owned("search_path").unwrap().as_str(),
            "GUC_RAW,public"
        );
        guc.begin_transaction();
        guc.set_config("search_path", Some(raw), true).unwrap();
        guc.commit_transaction();
        assert_eq!(
            guc.get_owned("search_path").unwrap().as_str(),
            "GUC_RAW,public"
        );
    });
}

#[test]
fn reset_xmloption_restores_the_connection_default_without_allocating() {
    let guc = GucState::new();
    let configs = [crate::storage::RoutineConfig {
        name: crate::storage::SqlName::parse("application_name").unwrap(),
        value: crate::util::StackStr::from_str("scoped"),
    }];
    crate::mem::guard::forbid_alloc(|| {
        guc.set_connection_default("xmloption", "document", guc::ConnectionDefaultSource::Role)
            .unwrap();
        guc.begin_transaction();
        guc.set_config("xmloption", Some("content"), false).unwrap();
        guc.commit_transaction();
        assert_eq!(guc.get_owned("xmloption").unwrap().as_str(), "content");
        guc.begin_transaction();
        guc.reset("xmloption").unwrap();
        guc.commit_transaction();
        assert_eq!(guc.get_owned("xmloption").unwrap().as_str(), "document");
        assert!(guc.render().xml_document);
        guc.begin_transaction();
        {
            let _scope = guc.enter_routine_configs(&configs).unwrap();
            guc.set_config("xmloption", Some("content"), false).unwrap();
        }
        assert_eq!(guc.get_owned("xmloption").unwrap().as_str(), "content");
        guc.commit_transaction();
        assert!(!guc.render().xml_document);
    });
}

#[test]
fn bound_builtin_casts_keep_indexed_point_reads() {
    let config = test_config("retained-query-indexed-casts");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE query_indexed_casts(id integer PRIMARY KEY,payload integer); \
         INSERT INTO query_indexed_casts \
         SELECT value,value*10 FROM generate_series(1,1024) AS source(value)",
        true,
    );
    session.success(&mut engine, "BEGIN", true);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT payload::text,-payload,payload+1 FROM query_indexed_casts WHERE id=511; \
             SELECT seq_scan,seq_tup_read,idx_scan,idx_tup_fetch \
             FROM pg_stat_xact_user_tables WHERE relname='query_indexed_casts'",
            true,
        )),
        ["5110|-5110|5111", "0|0|1|1"]
    );
    session.success(&mut engine, "ROLLBACK", true);
}
#[test]
fn application_names_escape_non_printable_bytes_without_allocating() {
    let guc = GucState::new();
    let exactly_full = "é".repeat(8);
    let full_expected = "\\xc3\\xa9".repeat(8);
    let overflowing = "é".repeat(9);
    crate::mem::guard::forbid_alloc(|| {
        for (value, expected) in [
            (" plain !~\\ ", " plain !~\\ "),
            ("a\tb", "a\\x09b"),
            (
                "\x01\t\n\x0b\x0c\r\x1b\x1f\x7f",
                "\\x01\\x09\\x0a\\x0b\\x0c\\x0d\\x1b\\x1f\\x7f",
            ),
            ("é中🦀", "\\xc3\\xa9\\xe4\\xb8\\xad\\xf0\\x9f\\xa6\\x80"),
            ("\\x09", "\\x09"),
        ] {
            assert_eq!(
                guc.set_config("application_name", Some(value), false)
                    .unwrap()
                    .as_str(),
                expected
            );
            assert_eq!(
                guc.get_owned("application_name").unwrap().as_str(),
                expected
            );
        }
        assert_eq!(
            guc.set_config("application_name", Some(&exactly_full), false)
                .unwrap()
                .as_str(),
            full_expected
        );
        assert_eq!(
            guc.set_config("application_name", Some(&overflowing), false)
                .unwrap_err()
                .sqlstate,
            "22023"
        );
        assert_eq!(
            guc.get_owned("application_name").unwrap().as_str(),
            full_expected
        );
        assert_eq!(
            guc.set_config("application_name", Some("zero\0byte"), false)
                .unwrap_err()
                .sqlstate,
            "22021"
        );
        assert_eq!(
            guc.get_owned("application_name").unwrap().as_str(),
            full_expected
        );
    });
}
#[test]
fn datestyle_assignments_preserve_reset_defaults_and_reject_conflicts_atomically() {
    let guc = GucState::new();
    guc.set_connection_default(
        "DateStyle",
        "SQL, YMD",
        guc::ConnectionDefaultSource::Database,
    )
    .unwrap();
    crate::mem::guard::forbid_alloc(|| {
        guc.begin_transaction();
        for (value, expected) in [
            ("", "German, DMY"),
            (" \t\n\u{b}", "German, DMY"),
            ("DEFAULT", "SQL, YMD"),
            ("DEFAULT,ISO", "ISO, YMD"),
            ("ISO,DEFAULT", "ISO, YMD"),
            ("DEFAULT,German", "German, DMY"),
            ("German,DEFAULT", "German, YMD"),
            ("MDY,DEFAULT,German", "German, MDY"),
            ("German,DEFAULT,MDY", "German, MDY"),
            ("ISO,ISO", "ISO, DMY"),
            ("\"ISO\", \"DMY\"", "ISO, DMY"),
            ("PostgreSQL", "Postgres, DMY"),
            ("European", "German, DMY"),
            ("US", "German, MDY"),
            ("NonEuro", "German, MDY"),
            ("German,MDY,German", "German, MDY"),
            ("DEFAULT,DEFAULT", "SQL, YMD"),
        ] {
            guc.set_config("DateStyle", Some("German,DMY"), false)
                .unwrap();
            assert_eq!(
                guc.set_config("DateStyle", Some(value), false)
                    .unwrap()
                    .as_str(),
                expected,
                "{value:?}"
            );
        }
        guc.set_config("DateStyle", Some("DEFAULT"), false).unwrap();
        assert_eq!(guc.get_owned("DateStyle").unwrap().as_str(), "SQL, YMD");
        guc.set_config("DateStyle", Some("German,MDY"), false)
            .unwrap();
        for value in [
            "ISO,SQL",
            "DMY,MDY",
            "ISO SQL",
            "ISO,",
            "ISO,,MDY",
            "\"ISO,DMY\"",
            "\"ISO\"suffix",
            "\"ISO",
            "\"IS\"\"O\"",
            "'DEFAULT'",
            "German,Euro,US",
            "ISO,SQL,DEFAULT",
            "SQL,DEFAULT,ISO",
        ] {
            assert_eq!(
                guc.set_config("DateStyle", Some(value), false)
                    .unwrap_err()
                    .sqlstate,
                "22023",
                "{value:?}"
            );
            assert_eq!(guc.get_owned("DateStyle").unwrap().as_str(), "German, MDY");
        }
        assert_eq!(
            guc.set_config("DateStyle", None, false).unwrap().as_str(),
            "SQL, YMD"
        );
        guc.commit_transaction();
    });
}

#[test]
fn datestyle_sql_values_apply_cumulative_components_and_quoted_defaults() {
    let config = test_config("decoded-datestyle-components");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SET DateStyle TO SQL,YMD; SELECT current_setting('DateStyle'); \
         SET DateStyle TO German; SELECT current_setting('DateStyle'); \
         SELECT set_config('DateStyle','German,DEFAULT',false); \
         SET DateStyle TO 'DEFAULT'; SELECT current_setting('DateStyle'); \
         SELECT set_config('DateStyle','\"SQL\",\"DMY\"',false); \
         SELECT set_config('DateStyle','',false)",
            true
        )),
        [
            "SQL, YMD",
            "German, DMY",
            "German, MDY",
            "ISO, MDY",
            "SQL, DMY",
            "SQL, DMY"
        ]
    );
    for sql in [
        "SET DateStyle TO ISO,SQL",
        "SELECT set_config('DateStyle','DMY,MDY',false)",
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            ["22023"]
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, "SELECT current_setting('DateStyle')", true)),
            ["SQL, DMY"]
        );
    }
}
