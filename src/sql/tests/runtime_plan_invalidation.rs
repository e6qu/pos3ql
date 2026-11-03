use super::*;

#[test]
fn discard_plans_inside_procedural_execution_rechecks_future_inputs() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("procedural-discard-plans-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE SEQUENCE discarded_checks; CREATE TYPE discarded_choice AS ENUM('a'); \
             CREATE FUNCTION discarded_allowed(value discarded_choice) RETURNS boolean LANGUAGE plpgsql AS $$ \
             BEGIN PERFORM nextval('discarded_checks'); RETURN true; END$$; \
             CREATE DOMAIN discarded_checked AS discarded_choice CHECK(discarded_allowed(VALUE)); \
             CREATE FUNCTION discarded_body() RETURNS integer LANGUAGE plpgsql AS $$ \
             BEGIN PERFORM cardinality('{a}'::discarded_checked[]); DISCARD PLANS; \
             RETURN cardinality('{a}'::discarded_checked[]); END$$", true);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT discarded_body(); SELECT last_value FROM discarded_checks",
                true
            )),
            ["1", "2"]
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT discarded_body(); SELECT last_value FROM discarded_checks",
                true
            )),
            ["1", "4"]
        );
        drop((session, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn empty_procedural_sites_leave_cursor_and_checked_input_receipts_in_order() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("empty-pl-site-receipts-{mode:?}"));
        config.transaction_mode = mode;
        config.routine_input_slots = 3;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut reader = ConfiguredTransactionSession::new(&config, &mut budget);
        let mut holder =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        reader.success(&mut engine,
            "CREATE TABLE empty_site_gate(id integer PRIMARY KEY,hits integer); INSERT INTO empty_site_gate VALUES(1,0); \
             CREATE SEQUENCE empty_site_checks; CREATE TYPE empty_site_choice AS ENUM('a'); \
             CREATE FUNCTION empty_site_allowed(value empty_site_choice) RETURNS boolean LANGUAGE plpgsql AS $$ \
             BEGIN PERFORM nextval('empty_site_checks'); RETURN true; END$$; \
             CREATE DOMAIN empty_site_checked AS empty_site_choice CHECK(empty_site_allowed(VALUE)); \
             CREATE FUNCTION empty_site_body() RETURNS integer LANGUAGE plpgsql AS $$ \
             DECLARE answer integer:=0; selected refcursor; BEGIN PERFORM 1; \
             EXECUTE 'SELECT 2' INTO answer; OPEN selected FOR SELECT 1; FETCH selected INTO answer; CLOSE selected; \
             PERFORM cardinality('{a}'::empty_site_checked[]); \
             UPDATE empty_site_gate SET hits=hits+1 WHERE id=1; RETURN answer; END$$; SET lock_timeout='10s'", true);
        holder.success(
            &mut engine,
            "BEGIN; UPDATE empty_site_gate SET hits=hits WHERE id=1",
            true,
        );
        let query = "SELECT empty_site_body()";
        assert!(matches!(
            reader.execute_status(&mut engine, query, true),
            ExecutionStatus::Blocked { .. }
        ));
        let deadline = reader.transaction.suspended_statement_deadline();
        assert!(deadline.is_some());
        let status = crate::mem::guard::forbid_alloc(|| {
            engine.execute_simple_from(
                query,
                0,
                &reader.arena,
                &mut reader.transaction,
                &mut reader.pool,
                &mut reader.cursors,
                &reader.guc,
                &mut Responder::new(&mut reader.send),
                1,
                false,
            )
        })
        .unwrap();
        assert!(matches!(status, ExecutionStatus::Blocked { .. }));
        assert_eq!(reader.transaction.suspended_statement_deadline(), deadline);
        holder.success(&mut engine, "ROLLBACK", true);
        let status = crate::mem::guard::forbid_alloc(|| {
            engine.execute_simple_from(
                query,
                0,
                &reader.arena,
                &mut reader.transaction,
                &mut reader.pool,
                &mut reader.cursors,
                &reader.guc,
                &mut Responder::new(&mut reader.send),
                1,
                false,
            )
        })
        .unwrap();
        assert_eq!(status, ExecutionStatus::Complete);
        assert_eq!(data_rows(reader.send.readable()), ["1"]);
        assert_eq!(
            data_rows(&holder.success(
                &mut engine,
                "SELECT last_value FROM empty_site_checks; SELECT hits FROM empty_site_gate",
                true
            )),
            ["1", "1"]
        );
        drop((reader, holder, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn caught_checked_input_errors_replay_before_a_later_row_lock_wait() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for language in ["sql", "plpgsql"] {
            let mut config = test_config(&format!("caught-input-error-wait-{mode:?}-{language}"));
            config.transaction_mode = mode;
            let mut budget = test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut reader = ConfiguredTransactionSession::new(&config, &mut budget);
            let mut holder =
                ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
            let mut schema_editor =
                ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(3);
            reader.success(&mut engine,
                "CREATE TABLE caught_gate(id integer PRIMARY KEY,hits integer); INSERT INTO caught_gate VALUES(1,0); \
                 CREATE SEQUENCE caught_checks; CREATE TYPE caught_choice AS ENUM('a'); \
                 CREATE FUNCTION caught_allowed(value caught_choice) RETURNS boolean LANGUAGE plpgsql AS $$ \
                 BEGIN PERFORM nextval('caught_checks'); RETURN false; END$$; \
                 CREATE DOMAIN caught_checked AS caught_choice CHECK(caught_allowed(VALUE)); \
                 SET check_function_bodies=off; SET lock_timeout='10s'", true);
            let body = match language {
                "sql" => "SELECT cardinality('{a}'::caught_checked[])",
                "plpgsql" => "BEGIN RETURN cardinality('{a}'::caught_checked[]); END",
                _ => unreachable!(),
            };
            reader.success(&mut engine, &format!(
                "CREATE FUNCTION caught_rejected() RETURNS integer LANGUAGE {language} AS $${body}$$; \
                 CREATE FUNCTION caught_body() RETURNS integer LANGUAGE plpgsql AS $$ \
                 DECLARE failures integer:=0; BEGIN FOR attempt IN 1..2 LOOP \
                 BEGIN PERFORM caught_rejected(); EXCEPTION WHEN check_violation THEN failures:=failures+1; END; \
                 END LOOP; UPDATE caught_gate SET hits=hits+1 WHERE id=1; RETURN failures; END$$; \
                 SET check_function_bodies=on"), true);
            holder.success(
                &mut engine,
                "BEGIN; UPDATE caught_gate SET hits=hits WHERE id=1",
                true,
            );
            let query = "SELECT caught_body()";
            let status = reader.execute_status(&mut engine, query, true);
            assert!(
                matches!(status, ExecutionStatus::Blocked { .. }),
                "{mode:?} {language}: {status:?}: {}",
                String::from_utf8_lossy(reader.send.readable())
            );
            schema_editor.success(&mut engine, "DROP DOMAIN caught_checked", true);
            let deadline = reader.transaction.suspended_statement_deadline();
            assert!(deadline.is_some());
            for _ in 0..2 {
                let status = crate::mem::guard::forbid_alloc(|| {
                    engine.execute_simple_from(
                        query,
                        0,
                        &reader.arena,
                        &mut reader.transaction,
                        &mut reader.pool,
                        &mut reader.cursors,
                        &reader.guc,
                        &mut Responder::new(&mut reader.send),
                        1,
                        false,
                    )
                })
                .unwrap();
                assert!(matches!(status, ExecutionStatus::Blocked { .. }));
                assert_eq!(reader.transaction.suspended_statement_deadline(), deadline);
                assert!(reader.send.is_empty());
            }
            assert_eq!(
                data_rows(&holder.success(
                    &mut engine,
                    "SELECT last_value FROM caught_checks; ROLLBACK",
                    true
                )),
                ["2"]
            );
            let status = crate::mem::guard::forbid_alloc(|| {
                engine.execute_simple_from(
                    query,
                    0,
                    &reader.arena,
                    &mut reader.transaction,
                    &mut reader.pool,
                    &mut reader.cursors,
                    &reader.guc,
                    &mut Responder::new(&mut reader.send),
                    1,
                    false,
                )
            })
            .unwrap();
            assert_eq!(status, ExecutionStatus::Complete);
            assert_eq!(data_rows(reader.send.readable()), ["2"]);
            assert!(error_sqlstates(reader.send.readable()).is_empty());
            assert_eq!(
                data_rows(&holder.success(
                    &mut engine,
                    "SELECT last_value FROM caught_checks; SELECT hits FROM caught_gate",
                    true
                )),
                ["2", "1"]
            );
            drop((reader, holder, schema_editor, engine));
            std::fs::remove_dir_all(config.data_dir).unwrap();
        }
    }
}

#[test]
fn cancelled_runtime_inputs_capture_fresh_path_dependencies() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for language in ["sql", "plpgsql"] {
            let mut config = test_config(&format!("cancelled-runtime-path-{mode:?}-{language}"));
            config.transaction_mode = mode;
            let mut budget = test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut reader = ConfiguredTransactionSession::new(&config, &mut budget);
            let mut holder =
                ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
            reader.success(&mut engine,
                "CREATE TABLE cancel_gate(id integer PRIMARY KEY,hits integer); INSERT INTO cancel_gate VALUES(1,0); \
                 CREATE SEQUENCE cancel_checks; CREATE TYPE cancel_choice AS ENUM('a'); \
                 CREATE FUNCTION cancel_allowed(value cancel_choice) RETURNS boolean LANGUAGE plpgsql AS $$ \
                 BEGIN PERFORM nextval('public.cancel_checks'); UPDATE public.cancel_gate SET hits=hits+1 WHERE id=1; RETURN true; END$$; \
                 CREATE DOMAIN cancel_checked AS cancel_choice CHECK(cancel_allowed(VALUE)); \
                 CREATE SCHEMA alternate; CREATE SEQUENCE alternate.cancel_checks; CREATE TYPE alternate.cancel_choice AS ENUM('a'); \
                 CREATE FUNCTION alternate.cancel_allowed(value alternate.cancel_choice) RETURNS boolean LANGUAGE plpgsql AS $$ \
                 BEGIN PERFORM nextval('alternate.cancel_checks'); RETURN true; END$$; \
                 CREATE DOMAIN alternate.cancel_checked AS alternate.cancel_choice CHECK(alternate.cancel_allowed(VALUE)); \
                 SET check_function_bodies=off", true);
            let body = match language {
                "sql" => "SELECT cardinality('{a}'::cancel_checked[])",
                "plpgsql" => "BEGIN RETURN cardinality('{a}'::cancel_checked[]); END",
                _ => unreachable!(),
            };
            reader.success(&mut engine, &format!("CREATE FUNCTION cancel_body() RETURNS integer LANGUAGE {language} AS $${body}$$; SET check_function_bodies=on"), true);
            holder.success(
                &mut engine,
                "BEGIN; UPDATE cancel_gate SET hits=hits WHERE id=1",
                true,
            );
            assert!(matches!(
                reader.execute_status(&mut engine, "SELECT cancel_body()", true),
                ExecutionStatus::Blocked { .. }
            ));
            crate::mem::guard::forbid_alloc(|| {
                engine.rollback_txn(&mut reader.transaction, &reader.guc)
            });
            assert!(!reader.guc.seq_session().retains_statement_effects());
            holder.success(&mut engine, "ROLLBACK", true);
            reader.success(&mut engine, "SET search_path=alternate,public", true);
            assert_eq!(
                data_rows(&reader.success(&mut engine, "SELECT public.cancel_body()", true)),
                ["1"]
            );
            assert_eq!(
                data_rows(&reader.success(
                    &mut engine,
                    "SELECT last_value FROM alternate.cancel_checks",
                    true
                )),
                ["1"]
            );
            reader.success(
                &mut engine,
                "ALTER DOMAIN alternate.cancel_checked ADD CONSTRAINT additional_check CHECK(true)",
                true,
            );
            assert_eq!(
                data_rows(&reader.success(&mut engine, "SELECT public.cancel_body()", true)),
                ["1"]
            );
            assert_eq!(
                data_rows(&reader.success(
                    &mut engine,
                    "SELECT last_value FROM alternate.cancel_checks",
                    true
                )),
                ["2"]
            );
            drop((reader, holder, engine));
            std::fs::remove_dir_all(config.data_dir).unwrap();
        }
    }
}

#[test]
fn extended_output_exhaustion_releases_checked_input_receipts_in_explicit_transactions() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("checked-input-output-exhaustion-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let mut output =
            crate::mem::FixedBuf::new(&mut budget, "checked input short response", 256).unwrap();
        session.success(&mut engine,
            "CREATE SEQUENCE overflow_checks; CREATE TYPE overflow_choice AS ENUM('a'); \
             CREATE FUNCTION overflow_allowed(value overflow_choice) RETURNS boolean LANGUAGE plpgsql AS $$ \
             BEGIN PERFORM nextval('overflow_checks'); RETURN true; END$$; \
             CREATE DOMAIN overflow_checked AS overflow_choice CHECK(overflow_allowed(VALUE)); \
             SET check_function_bodies=off; CREATE FUNCTION overflow_body() RETURNS integer \
             LANGUAGE sql SET search_path=public AS $$SELECT cardinality('{a}'::overflow_checked[])$$; \
             SET check_function_bodies=on; BEGIN", true);
        session.arena.reset();
        let status = crate::mem::guard::forbid_alloc(|| {
            engine.execute_extended(
                "SELECT overflow_body(),repeat('x',4096)",
                &session.arena,
                &[],
                &[],
                &mut session.transaction,
                &mut session.pool,
                &mut session.cursors,
                &session.guc,
                &mut Responder::for_execute(&mut output, crate::pg::respond::ResultFmt::ALL_TEXT),
                1,
                false,
            )
        })
        .unwrap();
        assert_eq!(status, ExtendedExecutionStatus::Complete(false));
        assert_eq!(error_sqlstates(output.readable()), ["54000"]);
        assert!(session.transaction.failed);
        assert!(!session.guc.seq_session().retains_statement_effects());
        assert!(session.transaction.suspended_statement_context().is_none());
        session.success(&mut engine, "ROLLBACK", true);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT last_value FROM overflow_checks",
                true
            )),
            ["1"]
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, "SELECT overflow_body()", true)),
            ["1"]
        );
        drop((session, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}
#[test]
fn runtime_input_retries_keep_their_receipts_across_namespace_changes() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for language in ["sql", "plpgsql"] {
            let mut config = test_config(&format!("runtime-plan-wait-{mode:?}-{language}"));
            config.transaction_mode = mode;
            let mut budget = test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut reader = ConfiguredTransactionSession::new(&config, &mut budget);
            let mut holder =
                ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
            let mut observer =
                ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(3);
            reader.success(&mut engine,
                "CREATE SEQUENCE runtime_wait_checks; \
                 CREATE TABLE runtime_wait_gate(id integer PRIMARY KEY,hits integer); \
                 INSERT INTO runtime_wait_gate VALUES(1,0); \
                 CREATE TYPE runtime_wait_choice AS ENUM('a'); \
                 CREATE FUNCTION runtime_wait_allowed(value runtime_wait_choice) RETURNS boolean \
                 LANGUAGE plpgsql AS $$BEGIN PERFORM nextval('runtime_wait_checks'); \
                 UPDATE runtime_wait_gate SET hits=hits+1 WHERE id=1; RETURN true; END$$; \
                 CREATE DOMAIN runtime_wait_checked AS runtime_wait_choice CHECK(runtime_wait_allowed(VALUE)); \
                 SET check_function_bodies=off; SET lock_timeout='10s'", true);
            let body = match language {
                "sql" => "SELECT cardinality('{a}'::runtime_wait_checked[])",
                "plpgsql" => "BEGIN RETURN cardinality('{a}'::runtime_wait_checked[]); END",
                _ => unreachable!(),
            };
            reader.success(&mut engine, &format!(
                "CREATE FUNCTION runtime_wait_body() RETURNS integer LANGUAGE {language} AS $${body}$$; \
                 SET check_function_bodies=on"), true);
            holder.success(
                &mut engine,
                "BEGIN; UPDATE runtime_wait_gate SET hits=hits WHERE id=1",
                true,
            );
            let query = "SELECT runtime_wait_body()";
            assert!(matches!(
                reader.execute_status(&mut engine, query, true),
                ExecutionStatus::Blocked { .. }
            ));
            assert!(reader.send.is_empty());
            let deadline = reader.transaction.suspended_statement_deadline();
            assert!(deadline.is_some());
            observer.success(&mut engine, "CREATE SCHEMA runtime_wait_unrelated", true);
            for _ in 0..2 {
                let status = crate::mem::guard::forbid_alloc(|| {
                    engine.execute_simple_from(
                        query,
                        0,
                        &reader.arena,
                        &mut reader.transaction,
                        &mut reader.pool,
                        &mut reader.cursors,
                        &reader.guc,
                        &mut Responder::new(&mut reader.send),
                        1,
                        false,
                    )
                })
                .unwrap();
                assert!(matches!(status, ExecutionStatus::Blocked { .. }));
                assert!(reader.send.is_empty());
                assert_eq!(reader.transaction.suspended_statement_deadline(), deadline);
            }
            assert_eq!(
                data_rows(&observer.success(
                    &mut engine,
                    "SELECT last_value,is_called FROM runtime_wait_checks",
                    true
                )),
                ["1|t"]
            );
            holder.success(&mut engine, "ROLLBACK", true);
            let status = crate::mem::guard::forbid_alloc(|| {
                engine.execute_simple_from(
                    query,
                    0,
                    &reader.arena,
                    &mut reader.transaction,
                    &mut reader.pool,
                    &mut reader.cursors,
                    &reader.guc,
                    &mut Responder::new(&mut reader.send),
                    1,
                    false,
                )
            })
            .unwrap();
            assert_eq!(status, ExecutionStatus::Complete);
            assert_eq!(data_rows(reader.send.readable()), ["1"]);
            assert!(error_sqlstates(reader.send.readable()).is_empty());
            assert_eq!(data_rows(&observer.success(&mut engine,
                "SELECT last_value FROM runtime_wait_checks; SELECT hits FROM runtime_wait_gate", true)),
                ["1", "1"]);
            assert!(reader.transaction.suspended_statement_deadline().is_none());
            drop((reader, holder, observer, engine));
            std::fs::remove_dir_all(config.data_dir).unwrap();
        }
    }
}

// PostgreSQL 18.6 two-session oracle SQL/Python SHA256:
// 0231e65b17050288153aa281bd19aa375bc53766324eee09b85efa159dbf8acf,
// 8ee89540b427084b6f5858a0c1c6de42288523fab4f7dacebe54fe6287bbaf0b.
#[test]
fn namespace_plan_invalidations_follow_local_commit_and_rollback_boundaries() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("namespace-plan-invalidation-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut first = ConfiguredTransactionSession::new(&config, &mut budget);
        let mut second =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        let replacement = TxnState::new_with_config(&mut budget, &config).unwrap();
        first.success(
            &mut engine,
            "CREATE SCHEMA global_probe; CREATE SEQUENCE global_checks; \
             CREATE TYPE global_choice AS ENUM('a'); \
             CREATE FUNCTION global_allowed(value global_choice) RETURNS boolean LANGUAGE plpgsql \
             AS $$BEGIN PERFORM nextval('global_checks'); RETURN true; END$$; \
             CREATE DOMAIN global_checked AS global_choice CHECK(global_allowed(VALUE)); \
             CREATE FUNCTION global_body() RETURNS global_checked[] LANGUAGE sql \
             SET search_path=public AS 'SELECT ''{a}''::global_checked[]'",
            true,
        );
        let counter = "SELECT last_value FROM global_checks";
        first.success(&mut engine, "SELECT global_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["2"]);
        first.success(
            &mut engine,
            "CREATE SCHEMA IF NOT EXISTS global_probe; ALTER SCHEMA global_probe OWNER TO postgres",
            true,
        );
        assert_eq!(
            error_sqlstates(&first.execute(&mut engine, "CREATE SCHEMA global_probe", true)),
            ["42P06"]
        );
        first.success(&mut engine, "SELECT global_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["2"]);

        second.success(&mut engine, "SELECT global_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["3"]
        );
        first.success(&mut engine, "BEGIN; CREATE SCHEMA global_pending", true);
        second.success(&mut engine, "SELECT global_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["3"]
        );
        first.success(&mut engine, "SELECT global_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["4"]);
        first.success(&mut engine, "COMMIT; SELECT global_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["5"]);
        second.success(&mut engine, "SELECT global_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["6"]
        );

        first.success(
            &mut engine,
            "BEGIN; CREATE SCHEMA global_rollback; SELECT global_body()",
            true,
        );
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["7"]);
        second.success(&mut engine, "SELECT global_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["7"]
        );
        first.success(&mut engine, "ROLLBACK; SELECT global_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["8"]);
        second.success(&mut engine, "SELECT global_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["8"]
        );

        // A fresh, preallocated transaction workspace keeps this backend's
        // plan lineage; temporary transaction buffers do not define a session.
        let retired = core::mem::replace(&mut first.transaction, replacement);
        first.success(&mut engine, "SELECT global_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["8"]);
        drop((retired, first, second, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn checks_that_create_namespaces_finish_one_input_plan_for_repeated_calls() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("checked-namespace-reentrant-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE SEQUENCE reentrant_checks; CREATE TYPE reentrant_choice AS ENUM('a'); \
             CREATE FUNCTION reentrant_allowed(value reentrant_choice) RETURNS boolean LANGUAGE plpgsql AS $$ \
             BEGIN PERFORM nextval('reentrant_checks'); CREATE SCHEMA IF NOT EXISTS reentrant_created; RETURN true; END$$; \
             CREATE DOMAIN reentrant_checked AS reentrant_choice CHECK(reentrant_allowed(VALUE)); \
             SET check_function_bodies=off; \
             CREATE FUNCTION reentrant_body() RETURNS reentrant_checked[] LANGUAGE sql SET search_path=public \
             AS 'SELECT ''{a}''::reentrant_checked[]'; SET check_function_bodies=on", true);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT reentrant_body(),reentrant_body()",
                true
            )),
            ["{a}|{a}"]
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT last_value FROM reentrant_checks",
                true
            )),
            ["1"]
        );
        session.success(&mut engine, "DROP SCHEMA reentrant_created", true);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT reentrant_body(),reentrant_body()",
                true
            )),
            ["{a}|{a}"]
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT last_value FROM reentrant_checks",
                true
            )),
            ["2"]
        );
        drop((session, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn procedural_sites_recheck_only_reached_statements_with_changed_dependencies() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("procedural-selected-plan-invalidation-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut first = ConfiguredTransactionSession::new(&config, &mut budget);
        let mut second =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        first.success(&mut engine,
            "CREATE SEQUENCE pl_valid_checks; CREATE TYPE pl_valid_choice AS ENUM('a','b'); \
             CREATE FUNCTION pl_valid_allowed(value pl_valid_choice) RETURNS boolean LANGUAGE plpgsql \
             AS $$BEGIN PERFORM nextval('pl_valid_checks'); RETURN true; END$$; \
             CREATE DOMAIN pl_valid_checked AS pl_valid_choice CHECK(pl_valid_allowed(VALUE)); \
             CREATE TABLE pl_valid_left(id integer); CREATE TABLE pl_valid_right(id integer); \
             INSERT INTO pl_valid_left VALUES(1); INSERT INTO pl_valid_right VALUES(1); \
             CREATE FUNCTION pl_valid_body(take_left boolean) RETURNS pl_valid_checked[] LANGUAGE plpgsql AS $$ \
             BEGIN IF take_left THEN RETURN (SELECT '{a}'::pl_valid_checked[] FROM pl_valid_left LIMIT 1); \
             ELSE RETURN (SELECT '{b}'::pl_valid_checked[] FROM pl_valid_right LIMIT 1); END IF; END$$; \
             CREATE FUNCTION pl_valid_static(take_left boolean) RETURNS pl_valid_checked[] LANGUAGE plpgsql AS $$ \
             DECLARE answer pl_valid_checked[]; BEGIN IF take_left THEN \
             SELECT '{a}'::pl_valid_checked[] INTO answer FROM pl_valid_left WHERE take_left LIMIT 1; ELSE \
             SELECT '{b}'::pl_valid_checked[] INTO answer FROM pl_valid_right WHERE NOT take_left LIMIT 1; END IF; \
             RETURN answer; END$$", true);
        let counter = "SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM pl_valid_checks";
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["0"]);
        first.success(
            &mut engine,
            "SELECT pl_valid_body(true); SELECT pl_valid_body(true)",
            true,
        );
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["1"]);
        first.success(
            &mut engine,
            "SELECT pl_valid_body(false); SELECT pl_valid_body(true)",
            true,
        );
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["2"]);
        first.success(
            &mut engine,
            "ALTER TABLE pl_valid_left ADD COLUMN note text",
            true,
        );
        first.success(&mut engine, "SELECT pl_valid_body(false)", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["2"]);
        first.success(
            &mut engine,
            "SELECT pl_valid_body(true); SELECT pl_valid_body(true)",
            true,
        );
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["3"]);

        second.success(&mut engine, "SELECT pl_valid_body(true)", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["4"]
        );
        second.success(
            &mut engine,
            "BEGIN; ALTER TABLE pl_valid_left ADD COLUMN pending text; SELECT pl_valid_body(true)",
            true,
        );
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["5"]
        );
        first.success(&mut engine, "SELECT pl_valid_body(true)", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["5"]);
        second.success(&mut engine, "ROLLBACK; SELECT pl_valid_body(true)", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["6"]
        );
        first.success(&mut engine, "SELECT pl_valid_body(true)", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["6"]);
        first.success(
            &mut engine,
            "SELECT pl_valid_static(true); SELECT pl_valid_static(false)",
            true,
        );
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["8"]);
        first.success(
            &mut engine,
            "ALTER TABLE pl_valid_left ADD COLUMN static_note text",
            true,
        );
        first.success(&mut engine, "SELECT pl_valid_static(false)", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["8"]);
        first.success(
            &mut engine,
            "SELECT pl_valid_static(true); SELECT pl_valid_static(true)",
            true,
        );
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["9"]);
        drop((first, second, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn role_catalog_invalidations_only_recheck_role_dependent_bodies() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("role-plan-invalidation-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut first = ConfiguredTransactionSession::new(&config, &mut budget);
        let mut second =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        first.success(
            &mut engine,
            "CREATE SEQUENCE role_checks; CREATE TYPE role_choice AS ENUM('a'); \
             CREATE FUNCTION role_allowed(value role_choice) RETURNS boolean LANGUAGE plpgsql \
             AS $$BEGIN PERFORM nextval('role_checks'); RETURN true; END$$; \
             CREATE DOMAIN role_checked AS role_choice CHECK(role_allowed(VALUE)); \
             CREATE TABLE role_anchor(id integer); INSERT INTO role_anchor VALUES(1); \
             ALTER TABLE role_anchor ENABLE ROW LEVEL SECURITY; CREATE ROLE role_reader; \
             CREATE FUNCTION role_body() RETURNS role_checked[] LANGUAGE sql \
             SET search_path=public AS 'SELECT ''{a}''::role_checked[] FROM role_anchor LIMIT 1'",
            true,
        );
        let counter = "SELECT last_value FROM role_checks";
        first.success(&mut engine, "SELECT role_body()", true);
        second.success(&mut engine, "SELECT role_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["3"]
        );
        first.success(&mut engine, "BEGIN; ALTER ROLE role_reader LOGIN", true);
        second.success(&mut engine, "SELECT role_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["3"]
        );
        first.success(&mut engine, "SELECT role_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["4"]);
        first.success(&mut engine, "COMMIT; SELECT role_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["5"]);
        second.success(&mut engine, "SELECT role_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["6"]
        );
        first.success(
            &mut engine,
            "ALTER ROLE role_reader LOGIN; SELECT role_body()",
            true,
        );
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["7"]);

        first.success(
            &mut engine,
            "ALTER TABLE role_anchor DISABLE ROW LEVEL SECURITY",
            true,
        );
        first.success(&mut engine, "SELECT role_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["8"]);
        second.success(&mut engine, "SELECT role_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["9"]
        );
        first.success(
            &mut engine,
            "BEGIN; ALTER ROLE role_reader NOLOGIN; SELECT role_body()",
            true,
        );
        second.success(&mut engine, "SELECT role_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["9"]
        );
        first.success(&mut engine, "COMMIT; SELECT role_body()", true);
        second.success(&mut engine, "SELECT role_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["9"]
        );
        drop((first, second, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn schema_owner_noops_preserve_permissions_and_real_changes_require_database_create() {
    let config = test_config("schema-owner-invalidation-noop");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE ROLE schema_reader; CREATE ROLE schema_successor; \
         GRANT schema_successor TO schema_reader; \
         CREATE SCHEMA owner_subject AUTHORIZATION schema_reader; \
         SET ROLE schema_successor; ALTER SCHEMA owner_subject OWNER TO schema_reader; RESET ROLE; \
         SET ROLE schema_reader",
        true,
    );
    let denied = session.execute(
        &mut engine,
        "ALTER SCHEMA owner_subject OWNER TO schema_successor",
        true,
    );
    assert_eq!(error_sqlstates(&denied), ["42501"]);
    assert!(String::from_utf8_lossy(&denied).contains("permission denied for database"));
    session.success(
        &mut engine,
        "RESET ROLE; GRANT CREATE ON DATABASE postgres TO schema_reader; SET ROLE schema_reader; \
         ALTER SCHEMA owner_subject OWNER TO schema_successor; SET ROLE schema_successor; \
         ALTER SCHEMA owner_subject OWNER TO schema_successor; RESET ROLE",
        true,
    );
    drop((session, engine));
    std::fs::remove_dir_all(config.data_dir).unwrap();
}

#[test]
fn prepared_catalog_work_detaches_local_plan_ownership_until_durable_resolution() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("prepared-plan-invalidation-{mode:?}"));
        config.transaction_mode = mode;
        config.max_prepared_transactions = 1;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut first = ConfiguredTransactionSession::new(&config, &mut budget);
        let mut second =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        first.success(&mut engine,
            "CREATE SEQUENCE prepared_checks; CREATE TYPE prepared_choice AS ENUM('a'); \
             CREATE FUNCTION prepared_allowed(value prepared_choice) RETURNS boolean LANGUAGE plpgsql \
             AS $$BEGIN PERFORM nextval('prepared_checks'); RETURN true; END$$; \
             CREATE DOMAIN prepared_checked AS prepared_choice CHECK(prepared_allowed(VALUE)); \
             CREATE TABLE prepared_rows(id integer); \
             CREATE FUNCTION prepared_body() RETURNS prepared_checked[] LANGUAGE sql \
             SET search_path=public AS 'SELECT ''{a}''::prepared_checked[]'", true);
        let counter = "SELECT last_value FROM prepared_checks";
        first.success(&mut engine, "SELECT prepared_body()", true);
        second.success(&mut engine, "SELECT prepared_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["3"]
        );
        first.success(&mut engine,
            "BEGIN; CREATE SCHEMA prepared_committed; SELECT prepared_body(); PREPARE TRANSACTION 'plan-commit'", true);
        first.success(&mut engine, "SELECT prepared_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["5"]);
        second.success(&mut engine, "SELECT prepared_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["5"]
        );
        second.success(&mut engine, "COMMIT PREPARED 'plan-commit'", true);
        second.success(&mut engine, "SELECT prepared_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["6"]
        );
        first.success(&mut engine, "SELECT prepared_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["7"]);

        first.success(&mut engine,
            "BEGIN; CREATE SCHEMA prepared_rolled_back; SELECT prepared_body(); PREPARE TRANSACTION 'plan-rollback'", true);
        first.success(&mut engine, "SELECT prepared_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["9"]);
        second.success(&mut engine, "SELECT prepared_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["9"]
        );
        second.success(&mut engine, "ROLLBACK PREPARED 'plan-rollback'", true);
        second.success(&mut engine, "SELECT prepared_body()", true);
        first.success(&mut engine, "SELECT prepared_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["9"]);
        first.success(
            &mut engine,
            "BEGIN; INSERT INTO prepared_rows VALUES(1); PREPARE TRANSACTION 'plan-data'",
            true,
        );
        first.success(&mut engine, "SELECT prepared_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["9"]);
        second.success(&mut engine, "ROLLBACK PREPARED 'plan-data'", true);
        first.success(&mut engine, "SELECT prepared_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["9"]);
        drop((first, second, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}
