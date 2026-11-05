use super::*;

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
