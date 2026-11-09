use super::*;

#[test]
fn function_bodies_keep_arguments_locals_writes_and_sequences_through_repeated_waits() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for rows in [false, true] {
            let mut config = test_config(&format!("function-body-progress-{mode:?}-{rows}"));
            config.transaction_mode = mode;
            config.execution_control_bytes = 64 << 10;
            config.execution_control_leases = 64;
            config.row_work_bytes = 128 << 10;
            config.row_work_leases = 64;
            let mut budget = test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut writer =
                ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
            let mut holder =
                ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
            let (signature, result, invocation, expected_length) = if rows {
                (
                    "RETURNS TABLE(payload text)",
                    "payload:=saved; RETURN NEXT; payload:=saved||':'||issued::text; RETURN NEXT; RETURN;",
                    "SELECT string_agg(payload,',') INTO answer FROM waiting_function(repeat('x',8192))",
                    16_387,
                )
            } else {
                (
                    "RETURNS text",
                    "RETURN saved||':'||issued::text;",
                    "SELECT waiting_function(repeat('x',8192)) INTO answer",
                    8_194,
                )
            };
            writer.success(&mut engine, &format!(
                "SET track_functions='all'; CREATE SEQUENCE function_progress_numbers; \
                 CREATE TABLE function_progress_rows(id integer); \
                 CREATE TABLE function_progress_audit(stage text,issued bigint,note text); \
                 CREATE FUNCTION waiting_function(p text) {signature} LANGUAGE plpgsql AS $$ \
                 DECLARE issued bigint:=nextval('function_progress_numbers'); saved text:=p; \
                 BEGIN INSERT INTO function_progress_audit VALUES('before',issued,saved); \
                 PERFORM pg_advisory_xact_lock(84); \
                 INSERT INTO function_progress_audit VALUES('after',issued,saved); {result} END$$; \
                 CREATE FUNCTION function_progress_trigger() RETURNS trigger LANGUAGE plpgsql AS $$ \
                 DECLARE answer text; BEGIN {invocation}; \
                 INSERT INTO function_progress_audit VALUES('caller',NEW.id,answer); RETURN NEW; END$$; \
                 CREATE TRIGGER function_progress_trigger AFTER INSERT ON function_progress_rows \
                 FOR EACH ROW EXECUTE FUNCTION function_progress_trigger()"
            ), true);
            holder.success(&mut engine, "BEGIN; SELECT pg_advisory_xact_lock(84)", true);
            let sql = "INSERT INTO function_progress_rows VALUES(1) RETURNING id";
            let mut status = writer.execute_status(&mut engine, sql, true);
            for _ in 0..3 {
                let ExecutionStatus::Blocked {
                    completed_statements,
                    output_mark,
                    wait,
                } = status
                else {
                    panic!(
                        "function did not retain its body: {mode:?}, set={rows}, {status:?}: {}",
                        String::from_utf8_lossy(writer.send.readable())
                    );
                };
                assert_eq!(wait, StatementWait::Lock);
                assert_eq!(data_rows(writer.send.readable()), ["1"]);
                assert!(command_tags(writer.send.readable()).is_empty());
                assert_eq!(
                    data_rows(&holder.success(
                        &mut engine,
                        "SELECT 40+2; SELECT last_value FROM function_progress_numbers",
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
                panic!("function stopped waiting before release")
            };
            assert_eq!(wait, StatementWait::Lock);
            holder.success(&mut engine, "COMMIT", true);
            writer.send.truncate_to(output_mark);
            writer.arena.recycle_front();
            let status = crate::mem::guard::forbid_alloc(|| {
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
            assert_eq!(status, ExecutionStatus::Complete, "{mode:?}, set={rows}");
            assert!(
                error_sqlstates(writer.send.readable()).is_empty(),
                "{}",
                String::from_utf8_lossy(writer.send.readable())
            );
            assert_eq!(data_rows(writer.send.readable()), ["1"]);
            assert_eq!(command_tags(writer.send.readable()), ["INSERT 0 1"]);
            assert_eq!(data_rows(&writer.success(&mut engine, "SELECT stage,issued,length(note) FROM function_progress_audit ORDER BY stage; SELECT calls FROM pg_stat_user_functions WHERE funcname='waiting_function'; SELECT last_value FROM function_progress_numbers", true)), ["after|1|8192".to_owned(), "before|1|8192".to_owned(), format!("caller|1|{expected_length}"), "1".to_owned(), "1".to_owned()]);
            writer.success(
                &mut engine,
                "INSERT INTO function_progress_rows VALUES(2)",
                true,
            );
            assert_eq!(data_rows(&writer.success(&mut engine, "SELECT count(*) FROM function_progress_audit; SELECT calls FROM pg_stat_user_functions WHERE funcname='waiting_function'; SELECT last_value FROM function_progress_numbers", true)), ["6", "2", "2"]);
            drop((holder, writer, engine));
            std::fs::remove_dir_all(config.data_dir).unwrap();
        }
    }
}

#[test]
fn paused_functions_keep_the_selected_body_handler_loop_and_settings() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("function-environment-progress-{mode:?}"));
        config.transaction_mode = mode;
        config.execution_control_bytes = 64 << 10;
        config.execution_control_leases = 64;
        config.row_work_bytes = 128 << 10;
        config.row_work_leases = 64;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut writer =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        let mut holder =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(&mut engine, "SET application_name='outer'; SET track_functions='all'; \
            CREATE SCHEMA function_home; CREATE SCHEMA other_client; \
            CREATE SEQUENCE function_environment_numbers; \
            CREATE TABLE function_environment_root(id integer); \
            CREATE TABLE function_environment_audit(stage text,value text,namespace text,app text); \
            CREATE FUNCTION public.environment_function(p text) RETURNS text LANGUAGE plpgsql \
            SECURITY DEFINER SET search_path=function_home,public SET application_name='callee' AS $$ \
            DECLARE n bigint:=nextval('public.function_environment_numbers'); saved text:=p; \
            BEGIN BEGIN RAISE EXCEPTION 'expected'; EXCEPTION WHEN raise_exception THEN \
                saved:=saved||':caught'; PERFORM pg_advisory_xact_lock(101); END; \
                FOR i IN 1..2 LOOP \
                    INSERT INTO public.function_environment_audit VALUES('loop'||i::text,saved||':'||n::text,current_schema,current_setting('application_name')); \
                    PERFORM pg_advisory_xact_lock(101+i); \
                END LOOP; RETURN saved; END$$; \
            CREATE FUNCTION function_environment_trigger() RETURNS trigger LANGUAGE plpgsql AS $$ \
            DECLARE answer text; BEGIN SELECT public.environment_function('payload') INTO answer; \
                INSERT INTO public.function_environment_audit VALUES('caller',answer,current_schema,current_setting('application_name')); RETURN NEW; END$$; \
            CREATE TRIGGER function_environment_trigger AFTER INSERT ON function_environment_root \
                FOR EACH ROW EXECUTE FUNCTION function_environment_trigger()", true);
        holder.success(&mut engine, "SET search_path=other_client,public; SET application_name='holder'; BEGIN; SELECT pg_advisory_lock(101),pg_advisory_lock(102),pg_advisory_lock(103)", true);
        let sql = "INSERT INTO function_environment_root VALUES(1)";
        let mut status = writer.execute_status(&mut engine, sql, true);
        holder.success(&mut engine, "CREATE OR REPLACE FUNCTION public.environment_function(p text) RETURNS text LANGUAGE plpgsql AS $$BEGIN RETURN 'replacement'; END$$", true);
        for key in [101, 102, 103] {
            for release in [false, true] {
                let ExecutionStatus::Blocked {
                    completed_statements,
                    output_mark,
                    wait,
                } = status
                else {
                    panic!(
                        "function lost its position at {key}, {release}: {status:?}: {}",
                        String::from_utf8_lossy(writer.send.readable())
                    );
                };
                assert_eq!(wait, StatementWait::Lock);
                assert_eq!(data_rows(&holder.success(&mut engine, "SELECT current_schema,current_setting('application_name'); SELECT last_value FROM public.function_environment_numbers", true)), ["other_client|holder", "1"]);
                if release {
                    assert_eq!(
                        data_rows(&holder.success(
                            &mut engine,
                            &format!("SELECT pg_advisory_unlock({key})"),
                            true
                        )),
                        ["t"]
                    );
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
        }
        assert_eq!(status, ExecutionStatus::Complete);
        assert!(
            error_sqlstates(writer.send.readable()).is_empty(),
            "{}",
            String::from_utf8_lossy(writer.send.readable())
        );
        assert_eq!(command_tags(writer.send.readable()), ["INSERT 0 1"]);
        holder.success(&mut engine, "COMMIT", true);
        assert_eq!(data_rows(&writer.success(&mut engine, "SELECT stage,value,namespace,app FROM function_environment_audit ORDER BY stage; SELECT current_schema,current_setting('application_name'); SELECT calls FROM pg_stat_user_functions WHERE funcname='environment_function'; SELECT last_value FROM function_environment_numbers", true)), ["caller|payload:caught|public|outer", "loop1|payload:caught:1|function_home|callee", "loop2|payload:caught:1|function_home|callee", "public|outer", "1", "1"]);
        writer.success(
            &mut engine,
            "INSERT INTO function_environment_root VALUES(2)",
            true,
        );
        assert_eq!(data_rows(&writer.success(&mut engine, "SELECT value FROM function_environment_audit WHERE stage='caller' ORDER BY value; SELECT last_value FROM function_environment_numbers", true)), ["payload:caught", "replacement", "1"]);
        drop((holder, writer, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn paused_function_capacity_failure_rolls_back_and_returns_its_reservations() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("function-body-exhaustion-{mode:?}"));
        config.transaction_mode = mode;
        config.execution_control_bytes = 64 << 10;
        config.execution_control_leases = 64;
        config.row_work_bytes = 128 << 10;
        config.row_work_leases = 64;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut writer =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        let mut holder =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(&mut engine, "CREATE SEQUENCE exhausted_function_numbers; \
            CREATE TABLE exhausted_function_root(id integer); CREATE TABLE exhausted_function_audit(n bigint); \
            CREATE FUNCTION exhausted_function() RETURNS bigint LANGUAGE plpgsql AS $$ \
            DECLARE n bigint:=nextval('exhausted_function_numbers'); BEGIN \
                INSERT INTO exhausted_function_audit VALUES(n); PERFORM pg_advisory_xact_lock(107); RETURN n; END$$; \
            CREATE FUNCTION exhausted_function_trigger() RETURNS trigger LANGUAGE plpgsql AS $$ \
            BEGIN PERFORM exhausted_function(); RETURN NEW; END$$; \
            CREATE TRIGGER exhausted_function_trigger AFTER INSERT ON exhausted_function_root \
            FOR EACH ROW EXECUTE FUNCTION exhausted_function_trigger()", true);
        let controls = std::sync::Arc::clone(&engine.storage.execution_control);
        for call in 1..=2 {
            holder.success(
                &mut engine,
                "BEGIN; SELECT pg_advisory_xact_lock(107)",
                true,
            );
            let sql = "INSERT INTO exhausted_function_root VALUES(1)";
            let ExecutionStatus::Blocked {
                completed_statements,
                output_mark,
                wait,
            } = writer.execute_status(&mut engine, sql, true)
            else {
                panic!("function did not wait before exhaustion");
            };
            assert_eq!(wait, StatementWait::Lock);
            let mut pressure = Vec::with_capacity(config.execution_control_leases);
            crate::mem::guard::forbid_alloc(|| {
                while let Ok(lease) = controls.lease::<u8>(1) {
                    assert!(pressure.len() < pressure.capacity());
                    pressure.push(lease);
                }
            });
            assert!(!pressure.is_empty());
            writer.send.truncate_to(output_mark);
            writer.arena.recycle_front();
            let status = crate::mem::guard::forbid_alloc(|| {
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
            assert_eq!(status, ExecutionStatus::Complete);
            assert_eq!(
                error_sqlstates(writer.send.readable()),
                ["54000"],
                "{}",
                String::from_utf8_lossy(writer.send.readable())
            );
            assert!(String::from_utf8_lossy(writer.send.readable()).contains("execution_control"));
            drop(pressure);
            holder.success(&mut engine, "COMMIT", true);
            assert_eq!(data_rows(&writer.success(&mut engine, "SELECT count(*) FROM exhausted_function_root; SELECT count(*) FROM exhausted_function_audit; SELECT last_value FROM exhausted_function_numbers", true)), ["0".to_owned(), "0".to_owned(), call.to_string()]);
        }
        writer.success(
            &mut engine,
            "INSERT INTO exhausted_function_root VALUES(2)",
            true,
        );
        assert_eq!(data_rows(&writer.success(&mut engine, "SELECT id FROM exhausted_function_root; SELECT n FROM exhausted_function_audit; SELECT last_value FROM exhausted_function_numbers", true)), ["2", "3", "3"]);
        drop((holder, writer, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn paused_positional_functions_keep_record_array_and_null_domain_arguments() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("function-argument-progress-{mode:?}"));
        config.transaction_mode = mode;
        config.execution_control_bytes = 64 << 10;
        config.execution_control_leases = 64;
        config.row_work_bytes = 128 << 10;
        config.row_work_leases = 64;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut writer =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        let mut holder =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(&mut engine, "CREATE TYPE function_pair AS(id integer,message text); \
            CREATE DOMAIN function_domain AS text; CREATE TABLE function_arguments_root(id integer); \
            CREATE TABLE function_arguments_audit(answer text); \
            CREATE FUNCTION positional_function(function_pair,integer[],function_domain) RETURNS text LANGUAGE plpgsql AS $$ \
            BEGIN PERFORM pg_advisory_xact_lock(114); \
                RETURN ($1).message||':'||$2[3]::text||':'||pg_typeof($3)::text; END$$; \
            CREATE FUNCTION function_arguments_trigger() RETURNS trigger LANGUAGE plpgsql AS $$ \
            DECLARE answer text; BEGIN SELECT positional_function(ROW(7,'payload')::function_pair,ARRAY[1,NULL,3],NULL::function_domain) INTO answer; \
                INSERT INTO function_arguments_audit VALUES(answer); RETURN NEW; END$$; \
            CREATE TRIGGER function_arguments_trigger AFTER INSERT ON function_arguments_root \
                FOR EACH ROW EXECUTE FUNCTION function_arguments_trigger()", true);
        holder.success(
            &mut engine,
            "BEGIN; SELECT pg_advisory_xact_lock(114)",
            true,
        );
        let sql = "INSERT INTO function_arguments_root VALUES(1)";
        let mut status = writer.execute_status(&mut engine, sql, true);
        for release in [false, true] {
            let ExecutionStatus::Blocked {
                completed_statements,
                output_mark,
                wait,
            } = status
            else {
                panic!(
                    "positional function did not retain arguments: {status:?}: {}",
                    String::from_utf8_lossy(writer.send.readable())
                );
            };
            assert_eq!(wait, StatementWait::Lock);
            if release {
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
        assert_eq!(
            data_rows(&writer.success(
                &mut engine,
                "SELECT answer FROM function_arguments_audit",
                true
            )),
            ["payload:3:function_domain"]
        );
        drop((holder, writer, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn paused_function_converts_its_return_before_restoring_the_callers_timezone() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("function-return-timezone-{mode:?}"));
        config.transaction_mode = mode;
        config.execution_control_bytes = 64 << 10;
        config.execution_control_leases = 64;
        config.row_work_bytes = 128 << 10;
        config.row_work_leases = 64;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut writer =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        let mut holder =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(&mut engine, "SET timezone='UTC'; CREATE TABLE function_timezone_root(id integer); \
            CREATE TABLE function_timezone_audit(answer timestamptz); \
            CREATE FUNCTION function_timezone() RETURNS timestamptz LANGUAGE plpgsql SET timezone='Europe/Bucharest' AS $$ \
            DECLARE answer text:='2026-01-01 00:00:00'; BEGIN PERFORM pg_advisory_xact_lock(115); RETURN answer; END$$; \
            CREATE FUNCTION function_timezone_trigger() RETURNS trigger LANGUAGE plpgsql AS $$ \
            DECLARE answer timestamptz; BEGIN SELECT function_timezone() INTO answer; INSERT INTO function_timezone_audit VALUES(answer); RETURN NEW; END$$; \
            CREATE TRIGGER function_timezone_trigger AFTER INSERT ON function_timezone_root FOR EACH ROW EXECUTE FUNCTION function_timezone_trigger()", true);
        holder.success(
            &mut engine,
            "BEGIN; SELECT pg_advisory_xact_lock(115)",
            true,
        );
        let sql = "INSERT INTO function_timezone_root VALUES(1)";
        let ExecutionStatus::Blocked {
            completed_statements,
            output_mark,
            wait,
        } = writer.execute_status(&mut engine, sql, true)
        else {
            panic!("timezone function did not retain its body");
        };
        assert_eq!(wait, StatementWait::Lock);
        holder.success(&mut engine, "COMMIT", true);
        writer.send.truncate_to(output_mark);
        writer.arena.recycle_front();
        let status = crate::mem::guard::forbid_alloc(|| {
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
        assert_eq!(status, ExecutionStatus::Complete);
        assert!(
            error_sqlstates(writer.send.readable()).is_empty(),
            "{}",
            String::from_utf8_lossy(writer.send.readable())
        );
        writer.success(
            &mut engine,
            "INSERT INTO function_timezone_root VALUES(2)",
            true,
        );
        assert_eq!(
            data_rows(&writer.success(
                &mut engine,
                "SELECT answer FROM function_timezone_audit ORDER BY answer; SHOW timezone",
                true
            )),
            ["2025-12-31 22:00:00+00", "2025-12-31 22:00:00+00", "UTC"]
        );
        drop((holder, writer, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn paused_initializers_and_domain_checks_retain_their_child_calls_and_values() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("function-initializer-progress-{mode:?}"));
        config.transaction_mode = mode;
        config.execution_control_bytes = 64 << 10;
        config.execution_control_leases = 64;
        config.row_work_bytes = 128 << 10;
        config.row_work_leases = 64;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut writer =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        let mut holder =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(&mut engine,"CREATE SEQUENCE initializer_numbers; \
            CREATE TABLE initializer_root(id integer); CREATE TABLE initializer_audit(stage text,id bigint,n bigint); \
            CREATE FUNCTION initializer_child(p integer) RETURNS bigint LANGUAGE plpgsql AS $$ \
                DECLARE n bigint:=nextval('initializer_numbers'); BEGIN INSERT INTO initializer_audit VALUES('child',p,n); \
                PERFORM pg_advisory_xact_lock(120+p); RETURN n; END$$; \
            CREATE FUNCTION initializer_check(n bigint) RETURNS boolean LANGUAGE plpgsql STRICT AS $$ \
                BEGIN INSERT INTO initializer_audit VALUES('check',n,n); PERFORM pg_advisory_xact_lock(123); RETURN n>0; END$$; \
            CREATE DOMAIN initializer_domain AS bigint CHECK(initializer_check(VALUE)); \
            CREATE FUNCTION initializer_parent() RETURNS bigint LANGUAGE plpgsql AS $$ \
                DECLARE first bigint:=initializer_child(1); second initializer_domain:=initializer_child(2); \
                BEGIN RETURN first+second; END$$; \
            CREATE FUNCTION initializer_trigger() RETURNS trigger LANGUAGE plpgsql AS $$ \
                DECLARE answer bigint; BEGIN SELECT initializer_parent() INTO answer; \
                INSERT INTO initializer_audit VALUES('caller',answer,answer); RETURN NEW; END$$; \
            CREATE TRIGGER initializer_trigger AFTER INSERT ON initializer_root FOR EACH ROW EXECUTE FUNCTION initializer_trigger()",true);
        holder.success(
            &mut engine,
            "BEGIN; SELECT pg_advisory_lock(121),pg_advisory_lock(122),pg_advisory_lock(123)",
            true,
        );
        let sql = "INSERT INTO initializer_root VALUES(1)";
        let mut status = writer.execute_status(&mut engine, sql, true);
        for (key, issued) in [(121, "1"), (122, "2"), (123, "2")] {
            for release in [false, true] {
                let ExecutionStatus::Blocked {
                    completed_statements,
                    output_mark,
                    wait,
                } = status
                else {
                    panic!(
                        "initializer lost progress at {key}: {status:?}: {}",
                        String::from_utf8_lossy(writer.send.readable())
                    );
                };
                assert_eq!(wait, StatementWait::Lock);
                assert_eq!(
                    data_rows(&holder.success(
                        &mut engine,
                        "SELECT last_value FROM initializer_numbers",
                        true
                    )),
                    [issued]
                );
                if release {
                    assert_eq!(
                        data_rows(&holder.success(
                            &mut engine,
                            &format!("SELECT pg_advisory_unlock({key})"),
                            true
                        )),
                        ["t"]
                    );
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
        }
        assert_eq!(status, ExecutionStatus::Complete);
        assert!(
            error_sqlstates(writer.send.readable()).is_empty(),
            "{}",
            String::from_utf8_lossy(writer.send.readable())
        );
        holder.success(&mut engine, "COMMIT", true);
        assert_eq!(data_rows(&writer.success(&mut engine,"SELECT stage,id,n FROM initializer_audit ORDER BY stage,id; SELECT last_value FROM initializer_numbers",true)),["caller|3|3","check|2|2","child|1|1","child|2|2","2"]);
        writer.success(&mut engine, "INSERT INTO initializer_root VALUES(2)", true);
        assert_eq!(data_rows(&writer.success(&mut engine,"SELECT count(*) FROM initializer_audit; SELECT last_value FROM initializer_numbers",true)),["8","4"]);
        drop((holder, writer, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn unowned_expression_waits_roll_back_instead_of_repeating_completed_child_writes() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for (site, body) in [
            (
                "assignment",
                "DECLARE n bigint; BEGIN n:=expression_child(); RETURN n; END",
            ),
            ("return", "BEGIN RETURN expression_child(); END"),
            (
                "bounds",
                "BEGIN FOR i IN 1..expression_child() LOOP NULL; END LOOP; RETURN 1; END",
            ),
            (
                "message",
                "BEGIN RAISE NOTICE '%',expression_child(); RETURN 1; END",
            ),
        ] {
            let mut config = test_config(&format!("function-expression-wait-{mode:?}-{site}"));
            config.transaction_mode = mode;
            config.execution_control_bytes = 64 << 10;
            config.execution_control_leases = 64;
            config.row_work_bytes = 128 << 10;
            config.row_work_leases = 64;
            let mut budget = test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut writer =
                ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
            let mut holder =
                ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
            writer.success(&mut engine,&format!("CREATE SEQUENCE expression_numbers; \
                CREATE TABLE expression_root(id integer); CREATE TABLE expression_audit(n bigint); \
                CREATE FUNCTION expression_child() RETURNS bigint LANGUAGE plpgsql AS $$ \
                    DECLARE n bigint:=nextval('expression_numbers'); BEGIN INSERT INTO expression_audit VALUES(n); \
                    PERFORM pg_advisory_xact_lock(125); RETURN n; END$$; \
                CREATE FUNCTION expression_parent() RETURNS bigint LANGUAGE plpgsql AS $${body}$$; \
                CREATE FUNCTION expression_trigger() RETURNS trigger LANGUAGE plpgsql AS $$ \
                    BEGIN PERFORM expression_parent(); RETURN NEW; END$$; \
                CREATE TRIGGER expression_trigger AFTER INSERT ON expression_root FOR EACH ROW EXECUTE FUNCTION expression_trigger()"),true);
            holder.success(
                &mut engine,
                "BEGIN; SELECT pg_advisory_xact_lock(125)",
                true,
            );
            let error = writer.execute(&mut engine, "INSERT INTO expression_root VALUES(1)", true);
            assert_eq!(
                error_sqlstates(&error),
                ["0A000"],
                "{mode:?} {site}: {}",
                String::from_utf8_lossy(&error)
            );
            assert!(
                String::from_utf8_lossy(&error)
                    .contains("expression wait requires an owned invocation log")
            );
            holder.success(&mut engine, "COMMIT", true);
            assert_eq!(data_rows(&writer.success(&mut engine,"SELECT count(*) FROM expression_root; SELECT count(*) FROM expression_audit; SELECT last_value FROM expression_numbers",true)),["0","0","1"]);
            writer.success(&mut engine, "INSERT INTO expression_root VALUES(2)", true);
            assert_eq!(data_rows(&writer.success(&mut engine,"SELECT id FROM expression_root; SELECT n FROM expression_audit; SELECT last_value FROM expression_numbers",true)),["2","2","2"]);
            drop((holder, writer, engine));
            std::fs::remove_dir_all(config.data_dir).unwrap();
        }
    }
}
