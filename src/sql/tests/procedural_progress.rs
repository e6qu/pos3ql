use super::*;

#[test]
fn wide_table_function_records_use_output_width_in_select_lists_and_from() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("wide-function-results-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let sql =
            include_str!("../../../tests/external/differential/249_wide_function_results.sql");
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            ["100", "0|100"],
            "{mode:?}"
        );
    }
}

#[test]
fn owned_set_results_reclaim_conversion_scratch_and_preserve_caught_prefixes() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("owned-set-results-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let sql = include_str!("../../../tests/external/differential/248_owned_set_results.sql");
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            [
                "4096|384|384",
                "0|first",
                "1|prefix",
                "3|caught",
                "0",
                "outside|1",
                "outside|3",
                "outside|3"
            ],
            "{mode:?}"
        );
    }
}

#[test]
fn loop_input_exhaustion_rolls_back_writes_and_releases_shared_capacity() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("owned-loop-capacity-{mode:?}"));
        config.transaction_mode = mode;
        config.row_work_bytes = 256;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(
            &mut engine,
            "CREATE TABLE loop_capacity_audit(n integer)",
            true,
        );
        let output = session.execute(&mut engine,
            "DO $$DECLARE v text; BEGIN INSERT INTO loop_capacity_audit VALUES(1); FOR v IN SELECT repeat('x',2048) LOOP RAISE EXCEPTION 'oversized input was admitted'; END LOOP; END$$", true);
        assert_eq!(error_sqlstates(&output), ["54000"], "{mode:?}");
        assert!(
            String::from_utf8_lossy(&output).contains("PL/pgSQL loop rows:"),
            "{mode:?}: {output:?}"
        );
        assert!(
            String::from_utf8_lossy(&output).contains("row_work_bytes"),
            "{mode:?}: {output:?}"
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT count(*) FROM loop_capacity_audit",
                true
            )),
            ["0"]
        );
        session.success(&mut engine,
            "DO $$DECLARE v text; BEGIN FOR v IN SELECT 'ok'::text LOOP INSERT INTO loop_capacity_audit VALUES(2); END LOOP; END$$", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, "SELECT n FROM loop_capacity_audit", true)),
            ["2"]
        );
    }
}

#[test]
fn owned_loop_inputs_preserve_records_empty_rows_and_once_only_effects() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("owned-loop-inputs-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let sql = include_str!("../../../tests/external/differential/247_owned_loop_rows.sql");
        // Notices survive a later error and identify the failing source without
        // changing the PostgreSQL reference fixture or its expected rows.
        let mut diagnostic_sql = sql.to_owned();
        for (source, stage) in [
            ("FOR r IN SELECT input.n", "static record"),
            ("FOR r IN EXECUTE", "dynamic record"),
            ("FOR r IN SELECT 99", "empty record"),
            ("FOR r IN SELECT source.n", "function record"),
            ("FOREACH item IN ARRAY", "empty array"),
        ] {
            diagnostic_sql = diagnostic_sql.replace(
                source,
                &format!("RAISE NOTICE 'loop input stage: {stage}'; {source}"),
            );
        }
        assert_eq!(
            data_rows(&session.success(&mut engine, &diagnostic_sql, true)),
            [
                "array_empty|42|12|f",
                "dynamic|four|7|t",
                "empty|<null>|7|f",
                "function|call:1|12|t",
                "saved|four|12|t",
                "static|two|3|t",
                "1",
            ],
            "{mode:?}"
        );
    }
}

#[test]
fn procedural_control_frames_preserve_nested_loops_handlers_and_returns() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("procedural-control-frames-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let sql =
            include_str!("../../../tests/external/differential/246_procedural_control_frames.sql");
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            [
                "case_else|ok",
                "case_match|ok",
                "case_null|20000",
                "case_searched|20000:case not found:CASE statement is missing ELSE part.",
                "case_simple|20000",
                "inner|23514",
                "outer|22012",
                "rethrow|22012",
                "summary|outside:237:22012:true",
                "2",
                "10|3,4",
                "done:1",
            ],
            "{mode:?}"
        );
        for statement in [
            "DO $$BEGIN CASE WHEN false THEN NULL; END CASE; END$$",
            "DO $$BEGIN CASE 1 WHEN 2 THEN NULL; END CASE; END$$",
            "DO $$BEGIN CASE NULL::integer WHEN NULL::integer THEN NULL; END CASE; END$$",
        ] {
            let output = session.execute(&mut engine, statement, true);
            assert_eq!(error_sqlstates(&output), ["20000"], "{mode:?}: {statement}");
            let hint = b"HCASE statement is missing ELSE part.\0";
            assert!(
                output.windows(hint.len()).any(|field| field == hint),
                "{mode:?}: {output:?}"
            );
        }
    }
}

#[test]
fn discarded_procedural_queries_reuse_memory_and_preserve_counts_and_effects() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("perform-loop-memory-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let sql = include_str!("../../../tests/external/differential/245_perform_loop_memory.sql");
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            ["2048|2048|32"],
            "{mode:?}"
        );
    }
}

#[test]
fn scalar_functions_in_from_preserve_out_columns_cardinality_and_effects() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("scalar-from-contract-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let sql =
            include_str!("../../../tests/external/differential/244_scalar_functions_in_from.sql");
        let output = session.success(&mut engine, sql, true);
        assert_eq!(
            data_rows(&output),
            [
                "2|row:2",
                "8|call:1",
                "9|call:2|1",
                "3|row:3|10|call:3|1",
                "2|2|row:2",
                "4|4|row:4",
                "NULL|NULL",
                "4",
                "11",
                "11",
                "12",
                "2",
                "NULL|NULL",
                "12|named:12",
                "NULL|NULL",
                "13|named:14",
                "15|wrapped:15",
                "NULL|NULL",
                "16|nested|17",
                "1|checked",
                "1",
                "21",
                "NULL",
            ],
            "{mode:?}"
        );
    }
}

#[test]
fn numeric_loop_frames_reuse_memory_and_restore_shadowed_locals_after_errors() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("reused-numeric-frames-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let sql = include_str!("../../../tests/external/differential/243_numeric_loop_frames.sql");
        let output = session.success(&mut engine, sql, true);
        assert_eq!(
            data_rows(&output),
            ["32767|outside|1|t", "outside|5", "returned:1:1"],
            "{mode:?}"
        );
    }
}

#[test]
fn expired_procedural_blocks_stop_before_empty_and_scalar_only_control_flow() {
    struct RestoreDeadline(i64);
    impl Drop for RestoreDeadline {
        fn drop(&mut self) {
            query::restore_statement_deadline(self.0);
        }
    }
    let programs = [
        "BEGIN END",
        "BEGIN NULL; END",
        "BEGIN FOR attempt IN 1..3 LOOP END LOOP; END",
        "BEGIN FOR attempt IN 1..3 LOOP NULL; END LOOP; END",
        "BEGIN WHILE false LOOP NULL; END LOOP; END",
        "BEGIN LOOP EXIT; END LOOP; END",
        "BEGIN IF true THEN NULL; ELSE NULL; END IF; END",
        "BEGIN CASE 1 WHEN 1 THEN NULL; ELSE NULL; END CASE; END",
        "BEGIN BEGIN RAISE SQLSTATE '57014'; EXCEPTION WHEN query_canceled THEN NULL; END; END",
        "BEGIN BEGIN RAISE SQLSTATE '57014'; EXCEPTION WHEN SQLSTATE '57014' THEN NULL; END; END",
    ];
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("procedural-control-deadlines-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine, "BEGIN", true);
        let restore = RestoreDeadline(query::statement_deadline());
        for source in programs {
            for deadline in [0, i64::MAX, i64::MIN] {
                session.arena.reset();
                session.send.clear();
                query::restore_statement_deadline(deadline);
                let result = crate::mem::guard::forbid_alloc(|| {
                    type_input_plans::with_owner(type_input_plans::Owner::Backend, || {
                        exec::execute_anonymous_plpgsql(
                            &mut engine,
                            &mut session.transaction,
                            &mut session.pool,
                            &mut session.cursors,
                            &session.guc,
                            exec::PlpgsqlTransactionContext::Atomic,
                            source,
                            &session.arena,
                            &mut Responder::new(&mut session.send),
                        )
                    })
                });
                if deadline == i64::MIN {
                    assert_eq!(result.unwrap_err().sqlstate, "57014", "{mode:?}: {source}");
                } else {
                    result.unwrap_or_else(|error| panic!("{mode:?}: {source}: {error:?}"));
                }
                assert_eq!(query::statement_deadline(), deadline);
            }
        }
        drop(restore);
        session.success(&mut engine, "ROLLBACK", true);
    }
}

#[test]
fn custom_sqlstates_remain_application_errors_through_handlers_and_routine_calls() {
    let codes = ["PZ001", "PZ002", "PZ003", "PZ004", "PZ999"];
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("application-control-codes-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(
            &mut engine,
            "CREATE TABLE application_codes_audit(code text, handler text)",
            true,
        );
        for code in codes {
            let direct = format!(
                "DO $$BEGIN RAISE SQLSTATE '{code}' USING MESSAGE='application error'; END$$"
            );
            let output = session.execute(&mut engine, &direct, true);
            assert_eq!(error_sqlstates(&output), [code], "{mode:?}: {direct}");
            assert!(session.transaction.suspended_statement_context().is_none());
            for (condition, handler) in [
                ("OTHERS".to_owned(), "others"),
                (format!("SQLSTATE '{code}'"), "exact"),
                ("SQLSTATE 'PZ000'".to_owned(), "class"),
            ] {
                let caught = format!(
                    "DO $$BEGIN BEGIN RAISE SQLSTATE '{code}' USING MESSAGE='application error'; EXCEPTION WHEN {condition} THEN INSERT INTO application_codes_audit VALUES('{code}','{handler}'); END; END$$"
                );
                session.success(&mut engine, &caught, true);
            }
            let routine = format!(
                "CREATE OR REPLACE FUNCTION application_code() RETURNS integer LANGUAGE plpgsql AS $$BEGIN RAISE SQLSTATE '{code}' USING MESSAGE='application routine error'; END$$"
            );
            session.success(&mut engine, &routine, true);
            let output = session.execute(&mut engine, "SELECT application_code()", true);
            assert_eq!(error_sqlstates(&output), [code], "{mode:?}: scalar {code}");
        }
        let mut expected = Vec::new();
        for code in codes {
            for handler in ["class", "exact", "others"] {
                expected.push(format!("{code}:{handler}"));
            }
        }
        let output = session.success(
            &mut engine,
            "SELECT code||':'||handler FROM application_codes_audit ORDER BY code,handler",
            true,
        );
        assert_eq!(data_rows(&output), expected);
        session.success(&mut engine, "CREATE FUNCTION application_code_rows() RETURNS SETOF integer LANGUAGE plpgsql AS $$BEGIN RETURN NEXT 1; RAISE SQLSTATE 'PZ004' USING MESSAGE='application row error'; END$$", true);
        let output = session.execute(&mut engine, "SELECT * FROM application_code_rows()", true);
        assert_eq!(error_sqlstates(&output), ["PZ004"]);
    }
}

#[test]
fn input_validation_reports_bad_data_but_propagates_domain_expression_errors() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("input-error-provenance-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(
            &mut engine,
            "CREATE DOMAIN positive_input AS integer CHECK(VALUE>0)",
            true,
        );
        for (sql, expected) in [
            ("SELECT pg_input_is_valid('1','positive_input')", "t"),
            ("SELECT pg_input_is_valid('-1','positive_input')", "f"),
            ("SELECT pg_input_is_valid('bad','positive_input')", "f"),
            (
                "SELECT sql_error_code FROM pg_input_error_info('-1','positive_input')",
                "23514",
            ),
            (
                "SELECT sql_error_code FROM pg_input_error_info('bad','positive_input')",
                "22P02",
            ),
        ] {
            assert_eq!(
                data_rows(&session.success(&mut engine, sql, true)),
                [expected],
                "{mode:?}: {sql}"
            );
        }
        session.success(
            &mut engine,
            "CREATE DOMAIN broken_input AS integer CHECK(VALUE/(VALUE-VALUE)>0)",
            true,
        );
        for sql in [
            "SELECT pg_input_is_valid('1','broken_input')",
            "SELECT sql_error_code FROM pg_input_error_info('1','broken_input')",
        ] {
            let output = session.execute(&mut engine, sql, true);
            assert_eq!(error_sqlstates(&output), ["22012"], "{mode:?}: {sql}");
        }
        session.success(&mut engine, "CREATE FUNCTION input_check(value integer) RETURNS boolean LANGUAGE plpgsql AS $$BEGIN RAISE EXCEPTION USING ERRCODE=current_setting('application_name'), MESSAGE='input expression failed'; END$$", true);
        session.success(
            &mut engine,
            "CREATE DOMAIN checked_input AS integer CHECK(input_check(VALUE))",
            true,
        );
        session.success(
            &mut engine,
            "CREATE TYPE input_record AS (value checked_input)",
            true,
        );
        for code in [
            "PZ001", "PZ002", "PZ003", "PZ004", "PZ999", "22012", "23514", "57014",
        ] {
            session.success(&mut engine, &format!("SET application_name='{code}'"), true);
            for (input, type_name) in [
                ("1", "checked_input"),
                ("{1}", "checked_input[]"),
                ("(1)", "input_record"),
            ] {
                for sql in [
                    format!("SELECT pg_input_is_valid('{input}','{type_name}')"),
                    format!(
                        "SELECT sql_error_code FROM pg_input_error_info('{input}','{type_name}')"
                    ),
                ] {
                    let output = session.execute(&mut engine, &sql, true);
                    assert_eq!(error_sqlstates(&output), [code], "{mode:?}: {sql}");
                    assert!(session.transaction.suspended_statement_context().is_none());
                }
            }
        }
    }
}
