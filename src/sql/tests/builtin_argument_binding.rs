use super::*;

#[test]
fn rule_definition_flags_preserve_formats_nulls_and_argument_effects() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config("rule-definition-input-binding");
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE TABLE rule_format_source(id integer); CREATE TABLE rule_format_sink(id integer); \
             CREATE RULE diag_nothing AS ON INSERT TO rule_format_source DO NOTHING; \
             CREATE RULE diag_instead AS ON DELETE TO rule_format_source DO INSTEAD NOTHING; \
             CREATE RULE diag_action AS ON UPDATE TO rule_format_source DO ALSO INSERT INTO rule_format_sink SELECT NEW.id LIMIT 1; \
             CREATE SEQUENCE rule_argument_effects", true);
        let plain =
            "CREATE RULE diag_nothing AS\n    ON INSERT TO public.rule_format_source DO NOTHING;";
        let pretty = "CREATE RULE diag_nothing AS\n    ON INSERT TO rule_format_source DO NOTHING;";
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT pg_get_ruledef(oid),pg_get_ruledef(oid,false),pg_get_ruledef(oid,true) \
               FROM pg_rewrite WHERE rulename='diag_nothing'",
                true
            )),
            [format!("{plain}|{plain}|{pretty}")]
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT pg_get_ruledef(oid,true) FROM pg_rewrite WHERE rulename='diag_instead'",
                true
            )),
            [
                "CREATE RULE diag_instead AS\n    ON DELETE TO rule_format_source DO INSTEAD NOTHING;"
            ]
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SET search_path=pg_catalog; \
             SELECT pg_get_ruledef(oid,true) FROM pg_rewrite WHERE rulename='diag_nothing'; \
             SET search_path=public,pg_catalog",
                true
            )),
            [plain]
        );
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT pg_get_ruledef(NULL::oid,nextval('rule_argument_effects')>0) IS NULL; \
             SELECT nextval('rule_argument_effects'); \
             ALTER SEQUENCE rule_argument_effects RESTART WITH 1; \
             SELECT pg_get_ruledef(nextval('rule_argument_effects')::oid,NULL) IS NULL; \
             SELECT nextval('rule_argument_effects'); \
             ALTER SEQUENCE rule_argument_effects RESTART WITH 1; \
             SELECT pg_get_ruledef(CASE WHEN nextval('rule_argument_effects')=1 THEN NULL::oid ELSE 0::oid END, \
                                   nextval('rule_argument_effects')>0) IS NULL; \
             SELECT nextval('rule_argument_effects'); \
             SELECT pg_get_ruledef(0::oid) IS NULL,pg_get_ruledef(0::oid,false) IS NULL,pg_get_ruledef(0::oid,true) IS NULL", true)),
            ["t", "1", "t", "1", "t", "3", "t|t|t"]);
        let bad_flag = session.execute(&mut engine, "SELECT pg_get_ruledef(NULL::oid,'bad')", true);
        assert_eq!(error_sqlstates(&bad_flag), ["22P02"]);
        session.success(
            &mut engine,
            "ALTER SEQUENCE rule_argument_effects RESTART WITH 1",
            true,
        );
        let unsupported = session.execute(
            &mut engine,
            "SELECT pg_get_ruledef(oid,nextval('rule_argument_effects')=1) \
               FROM pg_rewrite WHERE rulename='diag_action'",
            true,
        );
        assert_eq!(error_sqlstates(&unsupported), ["0A000"]);
        assert!(
            String::from_utf8_lossy(&unsupported)
                .contains("pretty formatting of rewrite rule actions is not supported")
        );
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT nextval('rule_argument_effects'); \
             SELECT pg_get_ruledef(oid,false) LIKE '%rule_format_sink%' FROM pg_rewrite WHERE rulename='diag_action'", true)),
            ["2", "t"]);
        session.success(&mut engine,
            "CREATE SCHEMA \"Rule Schema\"; CREATE TABLE \"Rule Schema\".\"Rule Source\"(id integer); \
             CREATE RULE \"Rule\"\"Name\" AS ON INSERT TO \"Rule Schema\".\"Rule Source\" DO NOTHING; \
             CREATE RULE \"select\" AS ON DELETE TO \"Rule Schema\".\"Rule Source\" DO INSTEAD NOTHING", true);
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT pg_get_ruledef(oid,false),pg_get_ruledef(oid,true) FROM pg_rewrite WHERE rulename='Rule\"Name'", true)),
            ["CREATE RULE \"Rule\"\"Name\" AS\n    ON INSERT TO \"Rule Schema\".\"Rule Source\" DO NOTHING;|CREATE RULE \"Rule\"\"Name\" AS\n    ON INSERT TO \"Rule Schema\".\"Rule Source\" DO NOTHING;"]);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT pg_get_ruledef(oid,false) FROM pg_rewrite WHERE rulename='select'",
                true
            )),
            [
                "CREATE RULE \"select\" AS\n    ON DELETE TO \"Rule Schema\".\"Rule Source\" DO INSTEAD NOTHING;"
            ]
        );
    }
}

#[test]
fn routine_lookup_diagnostics_keep_static_types_without_argument_effects() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config("routine-lookup-diagnostic-types");
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(
            &mut engine,
            "CREATE SEQUENCE diagnostic_effects; CREATE SCHEMA \"Hidden Schema\"; \
             CREATE TYPE \"Hidden Schema\".\"Odd Type\" AS ENUM('a'); \
             CREATE DOMAIN \"Hidden Schema\".\"Odd Domain\" AS integer; \
             CREATE TYPE \"Hidden Schema\".\"Odd Row\" AS(value integer); \
             CREATE TYPE public.\"select\" AS ENUM('a'); \
             CREATE FUNCTION diag_amb(value smallint) RETURNS integer LANGUAGE sql AS 'SELECT 1'; \
             CREATE FUNCTION diag_amb(value bigint) RETURNS integer LANGUAGE sql AS 'SELECT 1'; \
             CREATE TABLE diagnostic_inputs(value \"Hidden Schema\".\"Odd Type\"[]); \
             INSERT INTO diagnostic_inputs VALUES(NULL); \
             CREATE VIEW diagnostic_view AS SELECT value FROM diagnostic_inputs",
            true,
        );
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT enum_first(NULL::\"Hidden Schema\".\"Odd Type\"), \
                    enum_last(NULL::\"Hidden Schema\".\"Odd Type\"), \
                    enum_range(NULL::\"Hidden Schema\".\"Odd Type\"), \
                    enum_range(NULL::\"Hidden Schema\".\"Odd Type\",NULL::\"Hidden Schema\".\"Odd Type\")", true)),
            ["a|a|{a}|{a}"]);
        for (sql, state, message) in [
            (
                "SELECT diag_missing(1)",
                "42883",
                "function diag_missing(integer) does not exist",
            ),
            (
                "SELECT diag_missing('untyped')",
                "42883",
                "function diag_missing(unknown) does not exist",
            ),
            (
                "SELECT diag_missing(x=>nextval('diagnostic_effects'))",
                "42883",
                "function diag_missing(x => bigint) does not exist",
            ),
            (
                "SELECT * FROM diag_missing(nextval('diagnostic_effects'))",
                "42883",
                "function diag_missing(bigint) does not exist",
            ),
            (
                "SELECT \"Hidden Schema\".\"No Such Fn\"(1)",
                "42883",
                "function Hidden Schema.No Such Fn(integer) does not exist",
            ),
            (
                "SELECT diag_missing(nextval('diagnostic_effects'),NULL::\"Hidden Schema\".\"Odd Type\"[])",
                "42883",
                "function diag_missing(bigint, \"Hidden Schema\".\"Odd Type\"[]) does not exist",
            ),
            (
                "SELECT diag_missing(NULL::\"select\"[])",
                "42883",
                "function diag_missing(\"select\"[]) does not exist",
            ),
            (
                "SELECT diag_missing(NULL::\"Hidden Schema\".\"Odd Domain\"[])",
                "42883",
                "function diag_missing(\"Hidden Schema\".\"Odd Domain\"[]) does not exist",
            ),
            (
                "SELECT diag_missing(NULL::\"Hidden Schema\".\"Odd Row\"[])",
                "42883",
                "function diag_missing(\"Hidden Schema\".\"Odd Row\"[]) does not exist",
            ),
            (
                "SELECT diag_missing(NULL::diagnostic_inputs)",
                "42883",
                "function diag_missing(diagnostic_inputs) does not exist",
            ),
            (
                "SELECT diag_missing(NULL::diagnostic_inputs[])",
                "42883",
                "function diag_missing(diagnostic_inputs[]) does not exist",
            ),
            (
                "SELECT diag_missing(NULL::diagnostic_view)",
                "42883",
                "function diag_missing(diagnostic_view) does not exist",
            ),
            (
                "SELECT * FROM jsonb_populate_record(NULL::diagnostic_inputs,'{}'::json)",
                "42883",
                "function jsonb_populate_record(diagnostic_inputs, json) does not exist",
            ),
            (
                "SELECT * FROM json_populate_record(NULL::diagnostic_inputs,'{}'::jsonb)",
                "42883",
                "function json_populate_record(diagnostic_inputs, jsonb) does not exist",
            ),
            (
                "SELECT diag_missing(value) FROM diagnostic_inputs",
                "42883",
                "function diag_missing(\"Hidden Schema\".\"Odd Type\"[]) does not exist",
            ),
            (
                "SELECT diag_missing(source.value) FROM (SELECT value FROM diagnostic_inputs) source",
                "42883",
                "function diag_missing(\"Hidden Schema\".\"Odd Type\"[]) does not exist",
            ),
            (
                "SELECT diag_amb(NULL)",
                "42725",
                "function diag_amb(unknown) is not unique",
            ),
            (
                "SELECT * FROM diag_amb(NULL)",
                "42725",
                "function diag_amb(unknown) is not unique",
            ),
        ] {
            let output = session.execute(&mut engine, sql, true);
            assert_eq!(
                error_sqlstates(&output),
                [state],
                "{sql}: {}",
                String::from_utf8_lossy(&output)
            );
            let expected_field = format!("M{message}\0");
            assert!(
                output
                    .windows(expected_field.len())
                    .any(|field| field == expected_field.as_bytes()),
                "{sql}: {}",
                String::from_utf8_lossy(&output)
            );
        }
        let arguments = std::iter::repeat_n("nextval('diagnostic_effects')", 24)
            .collect::<Vec<_>>()
            .join(",");
        let output = session.execute(
            &mut engine,
            &format!("SELECT diag_missing({arguments})"),
            true,
        );
        // The whole signature is reported, as PostgreSQL reports it.
        assert_eq!(error_sqlstates(&output), ["42883"]);
        let signature = std::iter::repeat_n("bigint", 24)
            .collect::<Vec<_>>()
            .join(", ");
        assert!(String::from_utf8_lossy(&output).contains(&format!(
            "function diag_missing({signature}) does not exist"
        )));
        assert_eq!(
            data_rows(&session.success(&mut engine, "SELECT nextval('diagnostic_effects')", true)),
            ["1"]
        );
    }
}

#[test]
fn indirect_catalog_predicates_have_exact_typed_callable_contracts() {
    let config = test_config("indirect-catalog-predicate-contracts");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let mut rule_definitions = 0;
        for declaration in crate::sql::catalog::builtin_callable_signatures("pg_get_ruledef") {
            rule_definitions += 1;
            assert_eq!(declaration.result_oid, crate::sql::types::oid::TEXT);
            assert!(matches!(engine.storage.select_routine_call(
                "pg_catalog.pg_get_ruledef", &[], declaration.arguments, false, 0,
                crate::storage::RoutineCallKind::Function,
            ), crate::storage::RoutineCallSelection::Builtin { oid, .. } if oid == declaration.oid));
        }
        assert_eq!(rule_definitions, 2);
        for (name, _) in crate::sql::eval::funcs::system::CATALOG_PREDICATES {
            let mut count = 0;
            for declaration in crate::sql::catalog::builtin_callable_signatures(name) {
                count += 1;
                assert_eq!(declaration.schema, "pg_catalog");
                assert_eq!(declaration.result_oid, crate::sql::types::oid::BOOL);
                assert_eq!(declaration.kind, b'f');
                assert!(!declaration.returns_set);
                let qualified = crate::stack_format!(128, "pg_catalog.{}", name);
                assert!(!qualified.is_truncated());
                for written in [*name, qualified.as_str()] {
                    assert!(
                        matches!(engine.storage.select_routine_call(
                        written, &[], declaration.arguments, false, 0,
                        crate::storage::RoutineCallKind::Function,
                    ), crate::storage::RoutineCallSelection::Builtin { oid, .. } if oid == declaration.oid),
                        "{} OID {}",
                        written,
                        declaration.oid
                    );
                }
            }
            assert!(count > 0, "missing callable family {}", name);
        }
    });
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let output = session.success(
        &mut engine,
        "SELECT has_language_privilege('sql','USAGE'), \
                pg_catalog.has_language_privilege('sql','USAGE'), \
                has_language_privilege(current_user,'sql','USAGE'), \
                has_language_privilege(14::oid,'USAGE'), \
                has_function_privilege('pg_catalog.lower(text)','EXECUTE')",
        true,
    );
    assert_eq!(row_description_type_oids(&output), [16; 5]);
    assert_eq!(data_rows(&output), ["t|t|t|t|t"]);
}

#[test]
fn selected_builtin_arguments_preserve_defaults_names_and_once_only_effects() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config("selected-builtin-arguments");
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let fixture = include_str!(
            "../../../tests/external/differential_exact/63_builtin_argument_binding.sql"
        );
        let requests = fixture.split("-- next request").collect::<Vec<_>>();
        assert_eq!(
            data_rows(&session.success(&mut engine, requests[0], true)),
            [
                "107|integer",
                "1 year|interval",
                "00:00:00",
                "2 days",
                "Mixed,bare",
                "1 year 00:00:02",
                "3",
                "a",
                "a",
                "4",
                "t",
                "1",
                "t",
                "1",
            ]
        );
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, requests[1], true)),
            ["42883"]
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, requests[2], true)),
            ["1", "", "", "", "t|t|t|t|t|t", "null|null|t|t|t|t|t"]
        );
        assert_eq!(data_rows(&session.success(&mut engine,
            "CREATE SEQUENCE runtime_null_effects; \
             SELECT pg_catalog.make_interval(years=>CASE WHEN nextval('runtime_null_effects')=1 THEN NULL ELSE 1 END,secs=>nextval('runtime_null_effects')) IS NULL; \
             SELECT nextval('runtime_null_effects'); DROP SEQUENCE runtime_null_effects", true)), ["t", "3"]);

        let before = engine.storage.shared_statistics_reset_times();
        session.success(
            &mut engine,
            "SELECT pg_catalog.pg_stat_reset_shared(NULL)",
            true,
        );
        let explicit = engine.storage.shared_statistics_reset_times();
        assert!(explicit.background_writer > before.background_writer);
        assert_eq!(explicit.archiver, explicit.background_writer);
        assert_eq!(explicit.checkpointer, explicit.background_writer);
        assert_eq!(explicit.io, explicit.background_writer);
        assert_eq!(explicit.recovery_prefetch, explicit.background_writer);
        assert_eq!(explicit.wal, explicit.background_writer);
        assert_eq!(explicit.slru, [explicit.background_writer; 8]);
        session.success(
            &mut engine,
            "SELECT pg_catalog.pg_stat_reset_shared()",
            true,
        );
        let omitted = engine.storage.shared_statistics_reset_times();
        assert!(omitted.background_writer > explicit.background_writer);
        assert_eq!(omitted.archiver, omitted.background_writer);
        assert_eq!(omitted.checkpointer, omitted.background_writer);
        assert_eq!(omitted.io, omitted.background_writer);
        assert_eq!(omitted.recovery_prefetch, omitted.background_writer);
        assert_eq!(omitted.wal, omitted.background_writer);
        assert_eq!(omitted.slru, [omitted.background_writer; 8]);

        let vectors = session.success(
            &mut engine,
            "SELECT array_cat('1 2'::int2vector,'3 4'::int2vector), \
                    array_cat('1 2'::oidvector,'3 4'::oidvector)",
            true,
        );
        assert_eq!(row_description_type_oids(&vectors), [1005, 1028]);
        assert_eq!(data_rows(&vectors), ["[0:3]={1,2,3,4}|[0:3]={1,2,3,4}"]);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT array_dims(array_cat('1 2'::int2vector,'3 4'::int2vector)), \
                    array_dims(array_cat('1 2'::oidvector,'3 4'::oidvector)); \
             SELECT array_append('1 2'::int2vector,3::smallint), \
                    array_prepend(0::smallint,'1 2'::int2vector), \
                    array_position('1 2'::int2vector,2::smallint), \
                    array_positions('1 2'::int2vector,2::smallint), \
                    array_remove('1 2'::int2vector,2::smallint), \
                    array_replace('1 2'::int2vector,2::smallint,7::smallint); \
             CREATE SEQUENCE vector_binding_effects; \
             SELECT array_cat((nextval('vector_binding_effects')::text||' 2')::int2vector, \
                              (nextval('vector_binding_effects')::text||' 4')::int2vector); \
             SELECT nextval('vector_binding_effects'); DROP SEQUENCE vector_binding_effects",
                true
            )),
            [
                "[0:3]|[0:3]",
                "[0:2]={1,2,3}|[0:2]={0,1,2}|1|{1}|[0:0]={1}|[0:1]={1,7}",
                "[0:3]={1,2,2,4}",
                "3",
            ]
        );
    }
}

#[test]
fn explicit_variadic_keeps_nonvariadic_scalar_array_and_set_contracts() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config("explicit-variadic-fixed-contracts");
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE FUNCTION fixed_scalar(value integer) RETURNS integer LANGUAGE sql AS $$SELECT value$$; \
             CREATE FUNCTION fixed_array(value integer[]) RETURNS integer[] LANGUAGE sql AS $$SELECT value$$; \
             CREATE FUNCTION fixed_pair(marker integer,value integer[]) RETURNS integer LANGUAGE sql AS $$SELECT marker+cardinality(value)$$; \
             CREATE SEQUENCE variadic_fixed_effects", true);
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT fixed_scalar(VARIADIC 7),abs(VARIADIC(-3)); \
             SELECT fixed_array(VARIADIC ARRAY[1,2]),fixed_array(VARIADIC NULL),fixed_array(VARIADIC '{3,4}'); \
             SELECT fixed_array(VARIADIC value=>ARRAY[1,2]),fixed_pair(5,VARIADIC ARRAY[1,2]), \
                    fixed_pair(marker=>5,VARIADIC value=>ARRAY[1,2]); \
             SELECT to_json(VARIADIC ARRAY[1,2])::text,json_array_length(json_build_array(VARIADIC ARRAY[1,2])); \
             SELECT unnest(VARIADIC ARRAY[1,2]); \
             SELECT * FROM unnest(VARIADIC ARRAY[3,4]) AS selected(value); \
             SELECT fixed_scalar(VARIADIC nextval('variadic_fixed_effects')::integer); \
             SELECT nextval('variadic_fixed_effects'); \
             SELECT json_build_object(VARIADIC ARRAY['k','v'])->>'k', \
                    jsonb_build_object(VARIADIC ARRAY[]::text[])::text, \
                    json_build_array(VARIADIC NULL::integer[]) IS NULL; \
             SELECT json_array_length(json_build_array(VARIADIC ARRAY[nextval('variadic_fixed_effects'),nextval('variadic_fixed_effects')])); \
             SELECT nextval('variadic_fixed_effects')", true)),
            ["7|3", "{1,2}|NULL|{3,4}", "{1,2}|7|7", "[1,2]|2", "1", "2", "3", "4", "1", "2", "v|{}|t", "2", "5"]);
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, "SELECT unnest(VARIADIC 1)", true)),
            ["42883"]
        );
        assert_eq!(
            error_sqlstates(&session.execute(
                &mut engine,
                "SELECT json_build_object(VARIADIC ARRAY['k'])",
                true
            )),
            ["22023"]
        );
    }
}
