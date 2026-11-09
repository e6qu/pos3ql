use super::*;

#[test]
fn procedural_parameter_staging_uses_named_parameters_instead_of_all_declarations() {
    let mut declarations = String::new();
    for index in 0..128 {
        declarations.push_str(&format!("v{index} integer;"));
    }
    let scalar_body = format!("DECLARE {declarations} BEGIN v127 := p; RETURN v127; END");
    let table_body =
        format!("DECLARE {declarations} BEGIN v127 := p; answer := v127; RETURN NEXT; END");
    let procedure_body = format!("DECLARE {declarations} BEGIN v127 := p; answer := v127; END");
    let source_bytes = scalar_body
        .len()
        .max(table_body.len())
        .max(procedure_body.len());
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("owned-parameter-slots-{mode:?}"));
        config.transaction_mode = mode;
        let value_slots = 130 * core::mem::size_of::<Datum<'static>>();
        // Admit the original source, one local array and control metadata,
        // without room for a second declaration-sized parameter staging array.
        config.execution_control_bytes = (value_slots + source_bytes + 1024).next_multiple_of(64);
        config.execution_control_leases = 8;
        assert!(value_slots * 2 + source_bytes > config.execution_control_bytes);
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        session.success(&mut engine, &format!("CREATE FUNCTION named_scalar(p integer) RETURNS integer LANGUAGE plpgsql AS $${scalar_body}$$"), true);
        session.success(&mut engine, &format!("CREATE FUNCTION named_table(p integer) RETURNS TABLE(answer integer) LANGUAGE plpgsql AS $${table_body}$$"), true);
        session.success(&mut engine, &format!("CREATE PROCEDURE named_procedure(IN p integer, OUT answer integer) LANGUAGE plpgsql AS $${procedure_body}$$"), true);
        for sql in [
            "SELECT named_scalar(73)",
            "SELECT * FROM named_table(73)",
            "CALL named_procedure(73, NULL)",
        ] {
            assert_eq!(
                data_rows(&session.success(&mut engine, sql, true)),
                ["73"],
                "{mode:?}: {sql}"
            );
        }
    }
}

#[test]
fn procedural_source_capacity_precedes_initializers_and_returns_failed_call_storage() {
    let padding = " ".repeat(1400);
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("procedural-source-admission-{mode:?}"));
        config.transaction_mode = mode;
        config.execution_control_bytes = 1024;
        config.execution_control_leases = 16;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        session.success(
            &mut engine,
            "CREATE SEQUENCE source_initializer_calls",
            true,
        );
        session.success(&mut engine, &format!("CREATE FUNCTION denied_source() RETURNS integer LANGUAGE plpgsql AS $$DECLARE v integer := nextval('source_initializer_calls'); BEGIN {padding} RETURN v; END$$"), true);
        session.success(&mut engine, "CREATE FUNCTION admitted_source() RETURNS integer LANGUAGE plpgsql AS $$BEGIN RETURN 7; END$$", true);
        for _ in 0..2 {
            let error = session.execute(&mut engine, "SELECT denied_source()", true);
            assert_eq!(error_sqlstates(&error), ["54000"], "{mode:?}");
            assert!(String::from_utf8_lossy(&error).contains("PL/pgSQL source image"));
            assert!(String::from_utf8_lossy(&error).contains("execution_control_bytes"));
            assert_eq!(
                data_rows(&session.success(&mut engine, "SELECT admitted_source()", true)),
                ["7"],
                "{mode:?}"
            );
        }
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT nextval('source_initializer_calls')",
                true
            )),
            ["1"]
        );
    }
}

#[test]
fn procedural_source_keeps_the_original_call_after_its_definition_is_replaced() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("procedural-source-replacement-{mode:?}"));
        config.transaction_mode = mode;
        config.execution_control_bytes = 4096;
        config.execution_control_leases = 16;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        let sql = include_str!(
            "../../../tests/external/differential/263_procedural_source_replacement.sql"
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            ["original", "replacement"],
            "{mode:?}"
        );
    }
}

#[test]
fn incomplete_procedural_blocks_return_syntax_errors_and_release_source_storage() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("incomplete-procedural-source-{mode:?}"));
        config.transaction_mode = mode;
        config.execution_control_bytes = 1024;
        config.execution_control_leases = 16;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        session.success(&mut engine, "CREATE FUNCTION valid_source() RETURNS integer LANGUAGE plpgsql AS $$BEGIN RETURN 7; END$$", true);
        let sql = include_str!(
            "../../../tests/external/differential/264_incomplete_procedural_blocks.sql"
        );
        for statement in sql.lines().filter(|line| !line.trim().is_empty()) {
            let error = session.execute(&mut engine, statement, true);
            assert_eq!(error_sqlstates(&error), ["42601"], "{mode:?}: {statement}");
            assert!(String::from_utf8_lossy(&error).contains("syntax error at end of input"));
            assert_eq!(
                data_rows(&session.success(&mut engine, "SELECT valid_source()", true)),
                ["7"],
                "{mode:?}: {statement}"
            );
        }
    }
}

#[test]
fn control_capacity_is_admitted_before_procedural_initializer_effects() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for (capacity, bytes, leases) in [("bytes", 64, 8), ("leases", 4096, 3)] {
            let mut config = test_config(&format!("control-admission-{mode:?}-{capacity}"));
            config.transaction_mode = mode;
            config.execution_control_bytes = bytes;
            config.execution_control_leases = leases;
            let mut budget = test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut session =
                ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
            session.success(&mut engine, "CREATE SEQUENCE control_admission_calls", true);
            session.success(&mut engine, "CREATE FUNCTION denied_control() RETURNS integer LANGUAGE plpgsql AS $$DECLARE v integer := nextval('control_admission_calls'); BEGIN FOR i IN 1..1 LOOP NULL; END LOOP; RETURN v; END$$", true);
            let error = session.execute(&mut engine, "SELECT denied_control()", true);
            assert_eq!(error_sqlstates(&error), ["54000"]);
            assert!(String::from_utf8_lossy(&error).contains("execution_control_bytes"));
            assert_eq!(
                data_rows(&session.success(
                    &mut engine,
                    "SELECT nextval('control_admission_calls')",
                    true
                )),
                ["1"]
            );
        }
    }
}

#[test]
fn output_control_admission_precedes_initializers_and_releases_failed_calls() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("output-control-admission-{mode:?}"));
        config.transaction_mode = mode;
        config.execution_control_bytes = 4096;
        config.execution_control_leases = 4;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        session.success(&mut engine, "CREATE SEQUENCE output_admission_calls", true);
        session.success(&mut engine, "CREATE FUNCTION denied_output() RETURNS TABLE(answer integer) LANGUAGE plpgsql AS $$DECLARE v integer := nextval('output_admission_calls'); BEGIN answer := v; RETURN NEXT; END$$", true);
        session.success(&mut engine, "CREATE FUNCTION admitted_control() RETURNS integer LANGUAGE plpgsql AS $$BEGIN RETURN 7; END$$", true);
        for _ in 0..2 {
            let error = session.execute(&mut engine, "SELECT * FROM denied_output()", true);
            assert_eq!(error_sqlstates(&error), ["54000"]);
            assert!(String::from_utf8_lossy(&error).contains("execution_control_leases"));
            assert_eq!(
                data_rows(&session.success(&mut engine, "SELECT admitted_control()", true)),
                ["7"]
            );
        }
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT nextval('output_admission_calls')",
                true
            )),
            ["1"]
        );
    }
}

#[test]
fn saved_output_payload_exhaustion_rolls_back_and_returns_control_capacity() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("saved-output-payload-admission-{mode:?}"));
        config.transaction_mode = mode;
        config.execution_control_bytes = 4096;
        config.execution_control_leases = 16;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        session.success(&mut engine, "CREATE SEQUENCE saved_payload_calls", true);
        session.success(
            &mut engine,
            "CREATE TABLE saved_payload_audit(id integer)",
            true,
        );
        session.success(&mut engine, "CREATE FUNCTION saved_payload(p text) RETURNS TABLE(payload text) LANGUAGE plpgsql AS $$BEGIN payload := p; INSERT INTO saved_payload_audit VALUES (1); FOR payload IN 1..1 LOOP PERFORM nextval('saved_payload_calls'); RETURN NEXT; END LOOP; END$$", true);
        for expected in 1..=2 {
            let error = session.execute(
                &mut engine,
                "SELECT * FROM saved_payload(repeat('x',4096))",
                true,
            );
            assert_eq!(error_sqlstates(&error), ["54000"], "{mode:?}");
            assert!(String::from_utf8_lossy(&error).contains("execution_control_bytes"));
            assert_eq!(
                data_rows(&session.success(
                    &mut engine,
                    "SELECT count(*) FROM saved_payload_audit",
                    true
                )),
                [(expected - 1).to_string()],
                "{mode:?}: failed capture must roll back its earlier insert"
            );
            assert_eq!(
                data_rows(&session.success(&mut engine, "SELECT * FROM saved_payload('ok')", true)),
                ["ok"],
                "{mode:?}: failed capture must release its owned ranges"
            );
        }
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT count(*) FROM saved_payload_audit",
                true
            )),
            ["2"]
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, "SELECT nextval('saved_payload_calls')", true)),
            ["3"],
            "{mode:?}: denied capture must not enter the loop body"
        );
    }
}

#[test]
fn saved_output_scopes_reuse_values_and_preserve_nested_caught_and_typed_outputs() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("saved-output-scopes-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        let sql = include_str!("../../../tests/external/differential/261_saved_output_scopes.sql");
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            [
                "1537|384|384|1|1536|1181952",
                "initial|7",
                "initial|7",
                "NULL|7",
                "outer|[-2:-1]={7,NULL}|-2|1",
                "outer|[-2:-1]={7,NULL}|-2|2",
            ],
            "{mode:?}"
        );
    }
}

#[test]
fn parameter_names_and_shadowed_locals_preserve_input_and_output_scopes() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("procedural-parameter-scopes-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let sql = include_str!(
            "../../../tests/external/differential/259_procedural_parameter_scopes.sql"
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            [
                "7:integer:8:P0001:parameter handler:1:integer:7:8",
                "42",
                "7:8",
                "7|output",
                "NULL",
                "NULL",
                "7",
                "dynamic:1:integer:after:text",
            ],
            "{mode:?}"
        );
    }
}

#[test]
fn diagnostic_counters_and_handlers_preserve_the_innermost_binding() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("diagnostic-counter-scopes-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let sql =
            include_str!("../../../tests/external/differential/257_diagnostic_counter_scopes.sql");
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            ["1:integer|2:integer|3:integer|4:integer|P0001:caught:7:8:P0001:text:5:integer"],
            "{mode:?}"
        );
    }
}

#[test]
fn constant_targets_fail_before_body_effects_and_named_constant_cursors_open() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("procedural-constant-targets-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let parse = Arena::new(&mut budget, "constant-target fixture statements", 1 << 16).unwrap();
        let sql = include_str!(
            "../../../tests/external/differential/258_procedural_constant_targets.sql"
        );
        let mut parser = crate::sql::parser::Parser::new(sql, &parse).unwrap();
        let mut start = 0;
        let mut rows = Vec::new();
        let mut errors = Vec::new();
        while parser.next_stmt().unwrap().is_some() {
            let end = parser.source_position();
            let output = session.execute(&mut engine, &sql[start..end], true);
            start = end;
            rows.extend(data_rows(&output));
            errors.extend(error_sqlstates(&output).into_iter().map(str::to_owned));
        }
        assert_eq!(rows, ["0", "c", "1", "22005"], "{mode:?}");
        assert_eq!(
            errors,
            ["22005", "22005", "22005", "42601", "22005", "22005"],
            "{mode:?}"
        );
    }
}

#[test]
fn quoted_cursors_and_shadowed_variables_keep_postgresql_values_and_types() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("procedural-name-bindings-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let sql =
            include_str!("../../../tests/external/differential/255_procedural_name_bindings.sql");
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            [
                "first:second",
                "7:integer:42:integer:P0001:caught",
                "local:42:integer:outer state:outer message:42P01",
                "42P01:text:42",
            ],
            "{mode:?}"
        );
    }
}

#[test]
fn trigger_context_types_and_unavailable_names_match_postgresql() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("trigger-context-types-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let sql =
            include_str!("../../../tests/external/differential/254_trigger_context_types.sql");
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            [
                "name:oid:name:name:name:integer:text[]:text:text:text:true:NULL:42703",
                "name:oid:name:name:name:integer:text[]:text:text:text:false:first:42703"
            ],
            "{mode:?}"
        );
    }
}

#[test]
fn procedural_identifiers_preserve_quoted_case_and_fold_unquoted_names() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("procedural-identifier-case-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let sql =
            include_str!("../../../tests/external/differential/253_procedural_identifier_case.sql");
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            [
                "error-case:status-case",
                "lower:quoted:space",
                "3",
                "case_trigger:context-case",
                "lower:quoted"
            ],
            "{mode:?}"
        );
    }
}

#[test]
fn procedural_locals_initialize_once_in_order_after_seeded_parameters() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("local-initializer-layout-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let sql =
            include_str!("../../../tests/external/differential/252_local_initializer_layout.sql");
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            ["17|1:ok:true", "28|2:ok:true", "2"],
            "{mode:?}"
        );
    }
}

#[test]
fn lowered_assignments_bind_handler_variables_in_values_and_subscripts() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("handler-assignment-inputs-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let sql =
            include_str!("../../../tests/external/differential/251_handler_assignment_inputs.sql");
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            ["P0001", "P0001"],
            "{mode:?}"
        );
    }
}

#[test]
fn diagnostic_directory_admission_precedes_initializers_and_releases_failed_calls() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("diagnostic-directory-admission-{mode:?}"));
        config.transaction_mode = mode;
        config.execution_control_bytes = 4096;
        config.execution_control_leases = 4;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        session.success(
            &mut engine,
            "CREATE SEQUENCE diagnostic_initializer_calls",
            true,
        );
        session.success(&mut engine, "CREATE FUNCTION denied_diagnostic() RETURNS integer LANGUAGE plpgsql AS $$DECLARE v integer := nextval('diagnostic_initializer_calls'); BEGIN RETURN v; EXCEPTION WHEN OTHERS THEN RETURN -1; END$$", true);
        session.success(&mut engine, "CREATE FUNCTION admitted_diagnostic_control() RETURNS integer LANGUAGE plpgsql AS $$BEGIN RETURN 7; END$$", true);
        for _ in 0..2 {
            let error = session.execute(&mut engine, "SELECT denied_diagnostic()", true);
            assert_eq!(error_sqlstates(&error), ["54000"], "{mode:?}");
            assert!(String::from_utf8_lossy(&error).contains("PL/pgSQL diagnostics"));
            assert!(String::from_utf8_lossy(&error).contains("execution_control_leases"));
            assert_eq!(
                data_rows(&session.success(
                    &mut engine,
                    "SELECT admitted_diagnostic_control()",
                    true
                )),
                ["7"]
            );
        }
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT nextval('diagnostic_initializer_calls')",
                true
            )),
            ["1"]
        );
    }
}

#[test]
fn diagnostic_payload_exhaustion_rolls_back_without_entering_the_handler_and_reuses_capacity() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("diagnostic-payload-admission-{mode:?}"));
        config.transaction_mode = mode;
        config.execution_control_bytes = 4096;
        config.execution_control_leases = 16;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        session.success(
            &mut engine,
            "CREATE SEQUENCE diagnostic_handler_calls",
            true,
        );
        session.success(
            &mut engine,
            "CREATE TABLE diagnostic_payload_audit(id integer)",
            true,
        );
        session.success(&mut engine, "CREATE FUNCTION diagnostic_payload(p text) RETURNS text LANGUAGE plpgsql AS $$DECLARE detail text; BEGIN INSERT INTO diagnostic_payload_audit VALUES (1); BEGIN RAISE EXCEPTION 'caught' USING DETAIL = p; EXCEPTION WHEN OTHERS THEN PERFORM nextval('diagnostic_handler_calls'); GET STACKED DIAGNOSTICS detail = PG_EXCEPTION_DETAIL; RETURN SQLERRM || ':' || detail; END; END$$", true);
        for expected in 1..=2 {
            let error = session.execute(
                &mut engine,
                "SELECT diagnostic_payload(repeat('x',8192))",
                true,
            );
            assert_eq!(error_sqlstates(&error), ["54000"], "{mode:?}");
            assert!(String::from_utf8_lossy(&error).contains("PL/pgSQL diagnostics"));
            assert!(String::from_utf8_lossy(&error).contains("execution_control_bytes"));
            assert_eq!(
                data_rows(&session.success(
                    &mut engine,
                    "SELECT count(*) FROM diagnostic_payload_audit",
                    true
                )),
                [(expected - 1).to_string()],
                "{mode:?}"
            );
            assert_eq!(
                data_rows(&session.success(&mut engine, "SELECT diagnostic_payload('ok')", true)),
                ["caught:ok"],
                "{mode:?}"
            );
        }
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT count(*) FROM diagnostic_payload_audit",
                true
            )),
            ["2"]
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT nextval('diagnostic_handler_calls')",
                true
            )),
            ["3"],
            "{mode:?}: a failed capture must not enter its handler"
        );
    }
}

#[test]
fn nested_diagnostic_calls_return_capacity_and_keep_each_call_effect_once() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("nested-diagnostic-calls-{mode:?}"));
        config.transaction_mode = mode;
        config.execution_control_bytes = 32768;
        config.execution_control_leases = 32;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        let sql =
            include_str!("../../../tests/external/differential/262_nested_diagnostic_calls.sql");
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            ["outer 512:inner:8192", "512|131328|1|512", "513"],
            "{mode:?}"
        );
    }
}

#[test]
fn caught_diagnostics_reuse_storage_and_preserve_saved_and_nested_values() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("caught-diagnostic-reuse-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let sql =
            include_str!("../../../tests/external/differential/250_caught_diagnostic_reuse.sql");
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            [
                "4096:error 1",
                "outer:inner:outer:P0001:outer detail:outer hint:true:true:true"
            ],
            "{mode:?}"
        );
    }
}

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
fn nested_callback_results_reclaim_row_images_and_preserve_once_only_effects() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("nested-callback-results-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        let sql =
            include_str!("../../../tests/external/differential/260_nested_callback_results.sql");
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            [
                "4096:384:384|4096:384:384",
                "2|3|2",
                "4096:384:384",
                "3|6|3"
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
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
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

#[test]
fn checked_array_inputs_in_row_triggers_keep_inserted_rows_and_prior_effects() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("retained-trigger-input-{mode:?}"));
        config.transaction_mode = mode;
        config.execution_control_bytes = 64 << 10;
        config.execution_control_leases = 64;
        config.row_work_bytes = 128 << 10;
        config.row_work_leases = 64;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        let sql =
            include_str!("../../../tests/external/differential/265_trigger_input_visibility.sql");
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            ["1|3|1|1|1|1|8193|2"],
            "{mode:?}"
        );
    }
}

#[test]
fn retained_write_completion_keeps_returning_rows_and_trigger_effects_through_lock_waits() {
    for combined in [false, true] {
        for mode in [
            crate::config::TransactionMode::Mvcc,
            crate::config::TransactionMode::Locking,
        ] {
            let mut config = test_config(&format!("retained-write-completion-{mode:?}-{combined}"));
            config.transaction_mode = mode;
            config.execution_control_bytes = 64 << 10;
            config.execution_control_leases = 64;
            config.row_work_bytes = 128 << 10;
            config.row_work_leases = 64;
            let mut budget = test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
            let mut holder =
                ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
            let wait = if combined {
                "PERFORM pg_advisory_lock(41), nextval('held_expression_sequence'), pg_advisory_xact_lock(42);"
            } else {
                "PERFORM pg_advisory_lock(41); PERFORM pg_advisory_xact_lock(42);"
            };
            writer.success(
            &mut engine,
            &format!("CREATE SEQUENCE held_trigger_sequence; CREATE SEQUENCE held_expression_sequence; CREATE TABLE held_trigger_rows(id integer); \
             CREATE TABLE held_trigger_audit(stage text,id integer,n bigint,note text); \
             CREATE FUNCTION held_trigger() RETURNS trigger LANGUAGE plpgsql AS $$ \
             DECLARE n bigint:=nextval('held_trigger_sequence'); saved text:=repeat('x',8192); \
             BEGIN INSERT INTO held_trigger_audit VALUES('before',NEW.id,n,saved); \
             {wait} \
             saved:=saved||'!'; INSERT INTO held_trigger_audit VALUES('after',NEW.id,n,saved); \
             RETURN NEW; END$$; CREATE TRIGGER held_trigger AFTER INSERT ON held_trigger_rows \
             FOR EACH ROW EXECUTE FUNCTION held_trigger()"),
            true,
        );
            holder.success(&mut engine, "BEGIN; SELECT pg_advisory_xact_lock(42)", true);
            let sql = "INSERT INTO held_trigger_rows VALUES(1),(2) RETURNING id";
            let mut status = writer.execute_status(&mut engine, sql, true);
            for _ in 0..2 {
                let ExecutionStatus::Blocked {
                    completed_statements,
                    output_mark,
                    wait,
                } = status
                else {
                    panic!(
                        "trigger did not retain its write: {:?}: {}",
                        status,
                        String::from_utf8_lossy(writer.send.readable())
                    );
                };
                assert_eq!(wait, StatementWait::Lock);
                assert!(writer.transaction.has_pending_write_completion());
                assert_eq!(data_rows(writer.send.readable()), ["1", "2"]);
                assert!(command_tags(writer.send.readable()).is_empty());
                assert_eq!(
                    data_rows(&holder.success(
                        &mut engine,
                        "SELECT 40+2; SELECT last_value FROM held_trigger_sequence",
                        true
                    )),
                    ["42", "1"]
                );
                writer.send.truncate_to(output_mark);
                writer.arena.recycle_front();
                status = crate::mem::guard::forbid_alloc(|| {
                    engine.execute_simple_from(
                        sql,
                        completed_statements,
                        &writer.arena,
                        &mut writer.transaction,
                        &mut writer.pool,
                        &mut writer.cursors,
                        &writer.guc,
                        &mut Responder::new(&mut writer.send),
                        writer.connection_id,
                        false,
                    )
                })
                .unwrap();
            }
            let ExecutionStatus::Blocked {
                completed_statements,
                output_mark,
                wait,
            } = status
            else {
                panic!("trigger stopped waiting before its lock was released");
            };
            assert_eq!(wait, StatementWait::Lock);
            holder.success(&mut engine, "COMMIT", true);
            writer.send.truncate_to(output_mark);
            writer.arena.recycle_front();
            status = crate::mem::guard::forbid_alloc(|| {
                engine.execute_simple_from(
                    sql,
                    completed_statements,
                    &writer.arena,
                    &mut writer.transaction,
                    &mut writer.pool,
                    &mut writer.cursors,
                    &writer.guc,
                    &mut Responder::new(&mut writer.send),
                    writer.connection_id,
                    false,
                )
            })
            .unwrap();
            assert_eq!(status, ExecutionStatus::Complete, "{mode:?}");
            assert!(
                error_sqlstates(writer.send.readable()).is_empty(),
                "{}",
                String::from_utf8_lossy(writer.send.readable())
            );
            assert_eq!(data_rows(writer.send.readable()), ["1", "2"]);
            assert_eq!(command_tags(writer.send.readable()), ["INSERT 0 2"]);
            assert!(!writer.transaction.has_pending_write_completion());
            assert_eq!(
                data_rows(&writer.success(
                    &mut engine,
                    "SELECT stage,id,n,length(note) FROM held_trigger_audit ORDER BY n,stage; \
             SELECT last_value FROM held_trigger_sequence; \
            SELECT last_value,is_called FROM held_expression_sequence; \
             SELECT pg_advisory_unlock(41),pg_advisory_unlock(41),pg_advisory_unlock(41)",
                    true
                )),
                [
                    "after|1|1|8193",
                    "before|1|1|8192",
                    "after|2|2|8193",
                    "before|2|2|8192",
                    "2",
                    if combined { "2|t" } else { "1|f" },
                    "t|t|f"
                ]
            );
        }
    }
}

#[test]
fn retained_write_completion_keeps_lock_calls_around_completed_nested_functions() {
    for (label, fixture) in [
        (
            "perform",
            include_str!(
                "../../../tests/external/differential/276_suspended_query_invocations.sql"
            ),
        ),
        (
            "select-into",
            include_str!("../../../tests/external/differential/277_suspended_select_into.sql"),
        ),
    ] {
        for mode in [
            crate::config::TransactionMode::Mvcc,
            crate::config::TransactionMode::Locking,
        ] {
            let mut config = test_config(&format!("nested-expression-locks-{label}-{mode:?}"));
            config.transaction_mode = mode;
            config.execution_control_bytes = 64 << 10;
            config.execution_control_leases = 64;
            config.row_work_bytes = 128 << 10;
            config.row_work_leases = 64;
            let mut budget = test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
            let mut holder =
                ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
            let (setup, remainder) = fixture.split_once("-- Waitable write\n").unwrap();
            let (write, inspect) = remainder.split_once("-- Inspect results\n").unwrap();
            writer.success(&mut engine, setup, true);
            holder.success(&mut engine, "BEGIN; SELECT pg_advisory_xact_lock(42)", true);
            let mut status = writer.execute_status(&mut engine, write, true);
            for released in [false, false, true] {
                let ExecutionStatus::Blocked {
                    completed_statements,
                    output_mark,
                    wait,
                } = status
                else {
                    panic!(
                        "query failed to retain completed calls: {status:?}: {}",
                        String::from_utf8_lossy(writer.send.readable())
                    );
                };
                assert_eq!(wait, StatementWait::Lock);
                assert_eq!(
                    message_types(writer.send.readable())
                        .iter()
                        .filter(|kind| **kind == b'N')
                        .count(),
                    1,
                    "warning prefix changed across a procedural wait"
                );
                assert!(writer.transaction.has_pending_write_completion());
                assert_eq!(
                    data_rows(&holder.success(
                        &mut engine,
                        "SELECT 40+2; SELECT last_value FROM nested_expression_sequence",
                        true
                    )),
                    ["42", "2"]
                );
                if released {
                    holder.success(&mut engine, "COMMIT", true);
                }
                writer.send.truncate_to(output_mark);
                writer.arena.recycle_front();
                status = crate::mem::guard::forbid_alloc(|| {
                    engine.execute_simple_from(
                        write,
                        completed_statements,
                        &writer.arena,
                        &mut writer.transaction,
                        &mut writer.pool,
                        &mut writer.cursors,
                        &writer.guc,
                        &mut Responder::new(&mut writer.send),
                        writer.connection_id,
                        false,
                    )
                })
                .unwrap();
            }
            assert_eq!(
                status,
                ExecutionStatus::Complete,
                "{mode:?}: {}",
                String::from_utf8_lossy(writer.send.readable())
            );
            assert_eq!(
                message_types(writer.send.readable())
                    .iter()
                    .filter(|kind| **kind == b'N')
                    .count(),
                1,
                "warning repeated or disappeared after resumption"
            );
            assert!(error_sqlstates(writer.send.readable()).is_empty());
            assert_eq!(command_tags(writer.send.readable()), ["INSERT 0 1"]);
            assert!(!writer.transaction.has_pending_write_completion());
            let inspected = writer.success(&mut engine, inspect, true);
            assert_eq!(
                data_rows(&inspected),
                ["one,two,three|6|3|t|f|t|t|t|f|t|f"],
                "{mode:?}"
            );
            assert_eq!(
                message_types(&inspected)
                    .iter()
                    .filter(|kind| **kind == b'N')
                    .count(),
                3
            );
        }
    }
}

#[test]
fn retained_write_completion_distinguishes_transition_rows_from_catalog_dependencies() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("transition-write-dependencies-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let output = session.success(
            &mut engine,
            include_str!(
                "../../../tests/external/differential/280_transition_write_dependencies.sql"
            ),
            true,
        );
        assert_eq!(data_rows(&output), ["1|13"], "{mode:?}");
    }
}

#[test]
fn retained_write_completion_preserves_advisory_unlock_warnings_and_filtering() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("advisory-unlock-warnings-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let output = session.success(
            &mut engine,
            include_str!("../../../tests/external/differential/279_advisory_unlock_warnings.sql"),
            true,
        );
        assert_eq!(data_rows(&output), ["", "t", "f|f|f|f|t", "f"]);
        assert_eq!(
            message_types(&output)
                .iter()
                .filter(|kind| **kind == b'N')
                .count(),
            4
        );
        let wire = String::from_utf8_lossy(&output);
        for mode_name in ["ExclusiveLock", "ShareLock"] {
            assert_eq!(
                wire.matches(&format!(
                    "SWARNING\0VWARNING\0C01000\0Myou don't own a lock of type {mode_name}\0"
                ))
                .count(),
                2,
                "{mode:?}: {wire}"
            );
        }
    }
}

#[test]
fn retained_nested_write_completion_returns_once_to_its_waiting_parent() {
    for dynamic in [false, true] {
        for (event, seed, command, expected) in [
            (
                "UPDATE",
                "INSERT INTO nested_held_stock VALUES(1,0),(2,0);",
                "UPDATE nested_held_stock SET amount=amount+1;",
                "2|3|2",
            ),
            (
                "INSERT",
                "",
                "INSERT INTO nested_held_stock VALUES(3,0),(4,0);",
                "2|7|0",
            ),
            (
                "DELETE",
                "INSERT INTO nested_held_stock VALUES(1,0),(2,0);",
                "DELETE FROM nested_held_stock;",
                "0|NULL|NULL",
            ),
        ] {
            let command = if dynamic {
                format!("EXECUTE $command${command}$command$;")
            } else {
                command.to_owned()
            };
            for mode in [
                crate::config::TransactionMode::Mvcc,
                crate::config::TransactionMode::Locking,
            ] {
                let mut config = test_config(&format!(
                    "retained-nested-completion-{event}-{mode:?}-{dynamic}"
                ));
                config.transaction_mode = mode;
                config.execution_control_bytes = 64 << 10;
                config.execution_control_leases = 64;
                config.row_work_bytes = 128 << 10;
                config.row_work_leases = 64;
                let mut budget = test_engine_budget(&config, 1 << 29);
                let mut engine = Engine::new(&config, &mut budget).unwrap();
                let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
                let mut holder =
                    ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
                writer.success(&mut engine,
            &format!("CREATE SEQUENCE nested_held_sequence; CREATE TABLE nested_held_orders(id integer); \
             CREATE TABLE nested_held_stock(id integer,amount integer); \
             CREATE TABLE nested_held_audit(kind text,id integer,n bigint,note text); \
             {seed} \
             CREATE FUNCTION nested_held_child() RETURNS trigger LANGUAGE plpgsql AS $$ \
             DECLARE n bigint:=nextval('nested_held_sequence'); note text:=repeat('c',8192); row_id integer; \
             BEGIN IF TG_OP='DELETE' THEN row_id:=OLD.id; ELSE row_id:=NEW.id; END IF; \
             INSERT INTO nested_held_audit VALUES('child-before',row_id,n,note); \
             PERFORM pg_advisory_xact_lock(43); \
             INSERT INTO nested_held_audit VALUES('child-after',row_id,n,note||'!'); RETURN NEW; END$$; \
             CREATE TRIGGER nested_held_child AFTER {event} ON nested_held_stock \
             FOR EACH ROW EXECUTE FUNCTION nested_held_child(); \
             CREATE FUNCTION nested_held_parent() RETURNS trigger LANGUAGE plpgsql AS $$ \
             DECLARE changed bigint; note text:=repeat('p',8192); BEGIN \
             INSERT INTO nested_held_audit VALUES('parent-before',NEW.id,0,note); \
             {command} \
             GET DIAGNOSTICS changed=ROW_COUNT; \
             IF changed<>2 OR NOT FOUND THEN RAISE EXCEPTION 'nested write count changed'; END IF; \
             INSERT INTO nested_held_audit VALUES('parent-after',NEW.id,changed,note||'!'); \
             RETURN NEW; END$$; CREATE TRIGGER nested_held_parent AFTER INSERT ON nested_held_orders \
             FOR EACH ROW EXECUTE FUNCTION nested_held_parent()"), true);
                holder.success(&mut engine, "BEGIN; SELECT pg_advisory_xact_lock(43)", true);
                let sql = "INSERT INTO nested_held_orders VALUES(10) RETURNING id";
                let mut status = writer.execute_status(&mut engine, sql, true);
                for released in [false, false, true] {
                    let ExecutionStatus::Blocked {
                        completed_statements,
                        output_mark,
                        wait,
                    } = status
                    else {
                        panic!(
                            "nested write did not retain its parent: {:?}: {}",
                            status,
                            String::from_utf8_lossy(writer.send.readable())
                        );
                    };
                    assert_eq!(wait, StatementWait::Lock);
                    assert_eq!(data_rows(writer.send.readable()), ["10"]);
                    assert!(command_tags(writer.send.readable()).is_empty());
                    assert_eq!(
                        data_rows(&holder.success(
                            &mut engine,
                            "SELECT last_value FROM nested_held_sequence; SELECT 40+2",
                            true
                        )),
                        ["1", "42"]
                    );
                    if released {
                        holder.success(&mut engine, "COMMIT", true);
                    }
                    writer.send.truncate_to(output_mark);
                    writer.arena.recycle_front();
                    status = crate::mem::guard::forbid_alloc(|| {
                        engine.execute_simple_from(
                            sql,
                            completed_statements,
                            &writer.arena,
                            &mut writer.transaction,
                            &mut writer.pool,
                            &mut writer.cursors,
                            &writer.guc,
                            &mut Responder::new(&mut writer.send),
                            writer.connection_id,
                            false,
                        )
                    })
                    .unwrap();
                }
                assert_eq!(status, ExecutionStatus::Complete);
                assert!(
                    error_sqlstates(writer.send.readable()).is_empty(),
                    "{}",
                    String::from_utf8_lossy(writer.send.readable())
                );
                assert_eq!(data_rows(writer.send.readable()), ["10"]);
                assert_eq!(command_tags(writer.send.readable()), ["INSERT 0 1"]);
                assert_eq!(data_rows(&writer.success(&mut engine,
            "SELECT count(*),sum(id),sum(amount) FROM nested_held_stock; \
             SELECT kind,count(*),min(length(note)),max(length(note)) FROM nested_held_audit GROUP BY kind ORDER BY kind; \
             SELECT n FROM nested_held_audit WHERE kind='parent-after'; \
             SELECT last_value FROM nested_held_sequence", true)),
            [expected, "child-after|2|8193|8193", "child-before|2|8192|8192",
             "parent-after|1|8193|8193", "parent-before|1|8192|8192", "2", "2"]);
            }
        }
    }
}

#[test]
fn retained_nested_write_completion_keeps_dynamic_returned_records_and_bindings() {
    for (label, fixture, probe, expected_probe, expected) in [
        (
            "returned",
            include_str!("../../../tests/external/differential/273_dynamic_write_completion.sql"),
            "SELECT 40+2; SELECT (SELECT last_value FROM dynamic_held_bind),(SELECT last_value FROM dynamic_held_child)",
            ["42", "1|1"],
            "1|1|1|1|1|1|1|16385|1|1|t|integer",
        ),
        (
            "into-errors",
            include_str!("../../../tests/external/differential/275_dynamic_into_errors.sql"),
            "SELECT 40+2; SELECT last_value FROM into_edge_sequence",
            ["42", "1"],
            "0|3|0|1|1|42601/P0003/P0002|1|8193|t|integer",
        ),
    ] {
        let (setup, remainder) = fixture.split_once("-- Waitable write\n").unwrap();
        let (write, inspect) = remainder.split_once("-- Inspect results\n").unwrap();
        for mode in [
            crate::config::TransactionMode::Mvcc,
            crate::config::TransactionMode::Locking,
        ] {
            let mut config = test_config(&format!("dynamic-nested-{label}-{mode:?}"));
            config.transaction_mode = mode;
            config.execution_control_bytes = 64 << 10;
            config.execution_control_leases = 64;
            config.row_work_bytes = 128 << 10;
            config.row_work_leases = 64;
            let mut budget = test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
            let mut holder =
                ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
            writer.success(&mut engine, setup, true);
            holder.success(&mut engine, "BEGIN; SELECT pg_advisory_xact_lock(43)", true);
            let mut status = writer.execute_status(&mut engine, write, true);
            for released in [false, false, true] {
                let ExecutionStatus::Blocked {
                    completed_statements,
                    output_mark,
                    wait,
                } = status
                else {
                    panic!(
                        "dynamic write failed to retain returned values: {status:?}: {}",
                        String::from_utf8_lossy(writer.send.readable())
                    );
                };
                assert_eq!(wait, StatementWait::Lock);
                assert_eq!(data_rows(writer.send.readable()), ["10"]);
                assert!(command_tags(writer.send.readable()).is_empty());
                assert_eq!(
                    data_rows(&holder.success(&mut engine, probe, true)),
                    expected_probe
                );
                if released {
                    holder.success(&mut engine, "COMMIT", true);
                }
                writer.send.truncate_to(output_mark);
                writer.arena.recycle_front();
                status = crate::mem::guard::forbid_alloc(|| {
                    engine.execute_simple_from(
                        write,
                        completed_statements,
                        &writer.arena,
                        &mut writer.transaction,
                        &mut writer.pool,
                        &mut writer.cursors,
                        &writer.guc,
                        &mut Responder::new(&mut writer.send),
                        writer.connection_id,
                        false,
                    )
                })
                .unwrap();
            }
            assert_eq!(status, ExecutionStatus::Complete);
            assert!(
                error_sqlstates(writer.send.readable()).is_empty(),
                "{}",
                String::from_utf8_lossy(writer.send.readable())
            );
            assert_eq!(data_rows(writer.send.readable()), ["10"]);
            assert_eq!(command_tags(writer.send.readable()), ["INSERT 0 1"]);
            assert_eq!(
                data_rows(&writer.success(&mut engine, inspect, true)),
                [expected]
            );
        }
    }
}

#[test]
fn procedural_source_boundaries_follow_sql_quoting_comments_and_subscripts() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("procedural-lexical-boundaries-{mode:?}"));
        config.transaction_mode = mode;
        config.execution_control_bytes = 64 << 10;
        config.execution_control_leases = 64;
        config.row_work_bytes = 128 << 10;
        config.row_work_leases = 64;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        let sql = include_str!(
            "../../../tests/external/differential/274_procedural_lexical_boundaries.sql"
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            ["ok|branch|20|INTO, USING, THEN, LOOP|first,second|x|1"],
            "{mode:?}"
        );
    }
}

#[test]
fn utility_descriptions_match_execution() {
    let config = test_config("utility-descriptions");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE DOMAIN described_positive AS integer CHECK(VALUE>0)",
        true,
    );
    for sql in [
        "SELECT NULL::described_positive AS value",
        "SELECT (SELECT NULL::described_positive) AS value",
        "SELECT value FROM (SELECT NULL::described_positive AS value) s",
        "SELECT NULL::described_positive AS value UNION ALL SELECT NULL::described_positive WHERE false",
    ] {
        let described = describe_with(&mut engine, &mut budget, sql);
        let executed = session.success(&mut engine, sql, false);
        assert_eq!(
            row_description_type_oids(&described),
            [types::oid::INT4],
            "{sql}"
        );
        assert_eq!(
            row_description_type_oids(&executed),
            [types::oid::INT4],
            "{sql}"
        );
    }
    for (sql, names) in [
        ("SHOW TIMEZONE", &["TimeZone"][..]),
        ("SHOW DateStyle", &["DateStyle"][..]),
        ("SHOW IntervalStyle", &["IntervalStyle"][..]),
        ("SHOW application_name", &["application_name"][..]),
        ("SHOW ALL", &["name", "setting", "description"][..]),
    ] {
        let described = describe_with(&mut engine, &mut budget, sql);
        let executed = session.success(&mut engine, sql, false);
        assert_eq!(row_description_names(&described), names, "{sql}");
        assert_eq!(row_description_names(&executed), names, "{sql}");
        assert_eq!(
            row_description_type_oids(&described),
            row_description_type_oids(&executed),
            "{sql}"
        );
    }
}

#[test]
fn procedural_declared_records_keep_domain_types_in_stars_fields_and_calls() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("declared-record-types-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        let fixture =
            include_str!("../../../tests/external/differential/278_declared_record_types.sql");
        let final_query = fixture
            .find("SELECT check_declared_record_types()")
            .unwrap();
        let (setup, final_query) = fixture.split_at(final_query);
        session.success(&mut engine, setup, true);
        for query in [
            "SELECT * FROM record_source",
            "SELECT id,label FROM record_source",
            "SELECT t.id,t.label FROM record_source t",
            "SELECT public.record_source.id,public.record_source.label FROM public.record_source",
            "SELECT t.* FROM record_source t",
            "SELECT (t).* FROM record_source t",
            "SELECT f1 AS id,f2 AS label FROM (SELECT (ROW(7::record_positive,'seven'::record_label)).* FROM record_source) q",
            "SELECT record_return_positive() AS id,record_return_label() AS label",
            "SELECT record_return_positive() AS id,record_return_label() AS label FROM record_source",
            "SELECT (ARRAY[7::record_positive])[1] AS id,(ARRAY['seven'::record_label])[1] AS label",
            "SELECT (ROW(7,'seven')::record_pair).*",
            "SELECT (record_return_pair()).* FROM record_source",
            "SELECT (record_return_pair()).id AS id,(record_return_pair()).label AS label FROM record_source",
            "UPDATE record_source SET id=id RETURNING *",
            "UPDATE record_source SET id=id RETURNING (record_source).*",
            "UPDATE record_source AS s SET id=s.id RETURNING (s).*",
            "UPDATE record_source SET id=id RETURNING WITH(OLD AS before_row,NEW AS after_row) (before_row).*",
            "UPDATE record_source SET id=id RETURNING WITH(OLD AS before_row,NEW AS after_row) (after_row).*",
            "INSERT INTO record_field_copies VALUES(7,'seven') RETURNING (record_field_copies).*",
            "INSERT INTO record_conflict VALUES(7,'seven') ON CONFLICT(id) DO UPDATE SET label=excluded.label RETURNING (record_conflict).*",
        ] {
            let executed = session.success(&mut engine, query, false);
            assert_eq!(data_rows(&executed), ["7|seven"], "{mode:?}: {query}");
            assert_eq!(row_description_names(&executed), ["id", "label"], "{query}");
            assert_eq!(
                row_description_type_oids(&executed),
                [types::oid::INT4, types::oid::TEXT],
                "{query}"
            );
            let procedural = format!(
                "DO $$DECLARE result record; BEGIN EXECUTE '{}' INTO STRICT result; \
                 IF pg_typeof(result.id)<>'record_positive'::regtype \
                 OR pg_typeof(result.label)<>'record_label'::regtype \
                 OR result.id<>7 OR result.label<>'seven' THEN \
                 RAISE EXCEPTION 'declared record changed: types %, %, values %, %', \
                 pg_typeof(result.id),pg_typeof(result.label),result.id,result.label; END IF; END$$",
                query.replace('\'', "''")
            );
            session.success(&mut engine, &procedural, true);
        }
        for query in [
            "SELECT check_unknown_record_output()",
            "SELECT check_null_record_fields()",
            "SELECT check_following_record_fields()",
        ] {
            assert_eq!(
                data_rows(&session.success(&mut engine, query, true)),
                ["t"],
                "{mode:?}: {query}"
            );
        }
        assert_eq!(
            data_rows(&session.success(&mut engine, final_query, true)),
            ["t|1|text|text|t"],
            "{mode:?}"
        );
        for (query, types) in [
            (
                "SELECT * FROM record_source",
                [types::oid::INT4, types::oid::TEXT],
            ),
            (
                "SELECT (record_return_pair()).* FROM record_source",
                [types::oid::INT4, types::oid::TEXT],
            ),
            (
                "SELECT (ROW(NULL::text,'text'::text)).* FROM record_source",
                [types::oid::TEXT, types::oid::TEXT],
            ),
        ] {
            let described = describe_with(&mut engine, &mut budget, query);
            let executed = session.success(&mut engine, query, false);
            assert_eq!(row_description_type_oids(&described), types);
            assert_eq!(row_description_type_oids(&executed), types);
        }
        for query in [
            "SELECT (ROW(NULL,'text')).* FROM record_source",
            "SELECT (ROW(NULL,'text')).f1 FROM record_source",
            "SELECT (ROW(NULL,'text')).f2 FROM record_source",
        ] {
            let mut describe_budget = Budget::new(budget.remaining());
            let arena = Arena::new(&mut describe_budget, "record error describe", 2 << 20).unwrap();
            let mut described = crate::mem::FixedBuf::new(
                &mut describe_budget,
                "record error description",
                1 << 18,
            )
            .unwrap();
            let accepted = engine
                .describe(
                    query,
                    &arena,
                    &session.transaction,
                    None,
                    &mut Responder::for_describe(
                        &mut described,
                        crate::pg::respond::ResultFmt::ALL_TEXT,
                    ),
                    &[],
                )
                .unwrap();
            assert!(!accepted, "{query}");
            let executed = session.execute(&mut engine, query, false);
            assert_eq!(error_sqlstates(described.readable()), ["XX000"], "{query}");
            assert_eq!(error_sqlstates(&executed), ["XX000"], "{query}");
            assert!(command_tags(&executed).is_empty(), "{query}");
        }
    }
}

#[test]
fn whole_row_dependencies_keep_expanded_and_selected_columns() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("whole-row-dependencies-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                include_str!("../../../tests/external/differential/281_whole_row_dependencies.sql"),
                true,
            )),
            ["7|seven|7|7|7|7|7|seven|7|7|99"],
            "{mode:?}"
        );
    }
}

#[test]
fn record_projection_layout_keeps_sort_group_window_and_deferred_slots() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("record-projection-layout-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                include_str!(
                    "../../../tests/external/differential/282_record_projection_layout.sql"
                ),
                true,
            )),
            ["t|t|t|t|t|t|t|t|t|t", "2"],
            "{mode:?}"
        );
    }
}

#[test]
fn nested_static_and_dynamic_writes_preserve_enclosing_row_selection_and_rollback() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("nested-write-selection-{mode:?}"));
        config.transaction_mode = mode;
        config.execution_control_bytes = 64 << 10;
        config.execution_control_leases = 64;
        config.row_work_bytes = 128 << 10;
        config.row_work_leases = 64;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        let sql =
            include_str!("../../../tests/external/differential/266_nested_write_selection.sql");
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            ["63|396|123|141|10|0|2|3|8193|3|4"],
            "{mode:?}"
        );
    }
}

#[test]
fn uninitialized_domain_locals_validate_null_before_body_effects() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("domain-local-initialization-{mode:?}"));
        config.transaction_mode = mode;
        config.execution_control_bytes = 64 << 10;
        config.execution_control_leases = 64;
        config.row_work_bytes = 128 << 10;
        config.row_work_leases = 64;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        let sql = include_str!(
            "../../../tests/external/differential/267_domain_local_initialization.sql"
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            ["23502|23514|23502|23502|22004|0|7:true|2"],
            "{mode:?}"
        );
    }
}

#[test]
fn adjacent_triggers_observe_restored_settings_and_preserved_session_changes() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("trigger-setting-scope-{mode:?}"));
        config.transaction_mode = mode;
        config.execution_control_bytes = 64 << 10;
        config.execution_control_leases = 64;
        config.row_work_bytes = 128 << 10;
        config.row_work_leases = 64;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        let sql =
            include_str!("../../../tests/external/differential/271_trigger_setting_scope.sql");
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            ["10|5|t|t|t|t|t|outer|hex|warning|t"],
            "{mode:?}"
        );
    }
}

#[test]
fn write_completion_preserves_empty_actions_captured_rows_and_one_reply() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("write-completion-{mode:?}"));
        config.transaction_mode = mode;
        config.execution_control_bytes = 64 << 10;
        config.execution_control_leases = 64;
        config.row_work_bytes = 128 << 10;
        config.row_work_leases = 64;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        let sql = include_str!("../../../tests/external/differential/272_write_completion.sql");
        let output = session.success(&mut engine, sql, true);
        assert_eq!(data_rows(&output), ["2", "16|8|6|2|t|3|17"], "{mode:?}");
        let writes: Vec<_> = command_tags(&output)
            .into_iter()
            .filter(|tag| {
                ["INSERT ", "UPDATE ", "DELETE ", "MERGE ", "SELECT "]
                    .iter()
                    .any(|prefix| tag.starts_with(prefix))
            })
            .collect();
        assert_eq!(
            writes,
            [
                "INSERT 0 0",
                "UPDATE 0",
                "DELETE 0",
                "MERGE 0",
                "MERGE 1",
                "INSERT 0 1",
                "SELECT 1",
                "SELECT 1"
            ],
            "{mode:?}"
        );
    }
}

#[test]
fn mixed_writes_bind_each_row_trigger_to_its_own_transition_tables() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("mixed-transition-tables-{mode:?}"));
        config.transaction_mode = mode;
        config.execution_control_bytes = 64 << 10;
        config.execution_control_leases = 64;
        config.row_work_bytes = 128 << 10;
        config.row_work_leases = 64;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        let sql =
            include_str!("../../../tests/external/differential/270_mixed_transition_tables.sql");
        let output = session.success(&mut engine, sql, true);
        assert_eq!(data_rows(&output), ["10|4|4|2|t|t|301"], "{mode:?}");
        let writes: Vec<_> = command_tags(&output)
            .into_iter()
            .filter(|tag| tag.starts_with("INSERT ") || tag.starts_with("MERGE "))
            .collect();
        assert_eq!(writes, ["INSERT 0 3", "INSERT 0 2", "MERGE 3"], "{mode:?}");
    }
}

#[test]
fn trigger_parameters_remain_separate_from_prepared_write_parameters() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("trigger-parameter-scope-{mode:?}"));
        config.transaction_mode = mode;
        config.execution_control_bytes = 64 << 10;
        config.execution_control_leases = 64;
        config.row_work_bytes = 128 << 10;
        config.row_work_leases = 64;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        let sql =
            include_str!("../../../tests/external/differential/269_trigger_parameter_scope.sql");
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            ["6|2|2|2|t|t|15"],
            "{mode:?}"
        );
    }
}

#[test]
fn transition_columns_keep_declared_types_for_old_new_and_null_values() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("transition-column-types-{mode:?}"));
        config.transaction_mode = mode;
        config.execution_control_bytes = 64 << 10;
        config.execution_control_leases = 64;
        config.row_work_bytes = 128 << 10;
        config.row_work_leases = 64;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        let sql =
            include_str!("../../../tests/external/differential/268_transition_column_types.sql");
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            ["8|2|2|2|2|4|t"],
            "{mode:?}"
        );
    }
}

#[test]
fn unhandled_diagnostic_details_preserve_original_errors_at_small_control_capacity() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("unhandled-diagnostic-details-{mode:?}"));
        config.transaction_mode = mode;
        config.execution_control_bytes = 4096;
        config.execution_control_leases = 16;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        session.success(
            &mut engine,
            "CREATE TABLE unhandled_error_audit(n integer)",
            true,
        );
        session.success(&mut engine, "CREATE FUNCTION unhandled_detail(p text) RETURNS void LANGUAGE plpgsql AS $$BEGIN INSERT INTO unhandled_error_audit VALUES (1); RAISE EXCEPTION 'original error' USING DETAIL = p; END$$", true);
        let detail = "z".repeat(8192);
        for _ in 0..2 {
            let error = session.execute(
                &mut engine,
                "SELECT unhandled_detail(repeat('z',8192))",
                true,
            );
            assert_eq!(error_sqlstates(&error), ["P0001"], "{mode:?}");
            let text = String::from_utf8_lossy(&error);
            assert!(text.contains("original error"), "{mode:?}: {text}");
            assert!(
                text.contains(&detail),
                "{mode:?}: original detail must survive"
            );
            assert_eq!(
                data_rows(&session.success(
                    &mut engine,
                    "SELECT count(*) FROM unhandled_error_audit",
                    true,
                )),
                ["0"],
                "{mode:?}: the failed call must roll back its write"
            );
        }
    }
}
