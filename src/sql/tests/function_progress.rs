use super::*;

fn resume_function_progress_wait(
    engine: &mut Engine,
    writer: &mut ConfiguredTransactionSession,
    sql: &str,
    status: ExecutionStatus,
) -> ExecutionStatus {
    let ExecutionStatus::Blocked {
        completed_statements,
        output_mark,
        wait,
    } = status
    else {
        panic!(
            "function stopped waiting: {status:?}: {}",
            String::from_utf8_lossy(writer.send.readable())
        );
    };
    assert_eq!(wait, StatementWait::Lock);
    output_mark.truncate_unsent(&mut writer.send);
    writer.arena.recycle_front();
    crate::mem::guard::forbid_alloc(|| {
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
    .unwrap()
}

#[test]
fn saved_advisory_unlock_results_survive_later_function_calls() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("advisory-result-progress-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut writer =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        writer.success(
            &mut engine,
            "CREATE SEQUENCE advisory_result_numbers; \
            CREATE FUNCTION advisory_result_child() RETURNS bigint LANGUAGE plpgsql AS $$ \
                BEGIN RETURN nextval('advisory_result_numbers'); END$$; \
            SELECT pg_advisory_lock(301),pg_advisory_lock_shared(302), \
                pg_advisory_lock(303,1),pg_advisory_lock_shared(304,1)",
            true,
        );
        assert_eq!(
            data_rows(&writer.success(
                &mut engine,
                "SELECT pg_advisory_unlock(301),pg_advisory_unlock_shared(302), \
                pg_advisory_unlock(303,1),pg_advisory_unlock_shared(304,1),advisory_result_child()",
                true
            )),
            ["t|t|t|t|1"]
        );
        writer.success(&mut engine, "SELECT pg_advisory_lock(305)", true);
        assert_eq!(data_rows(&writer.success(&mut engine,
            "SELECT pg_advisory_unlock_all(),advisory_result_child(); SELECT last_value FROM advisory_result_numbers", true)), ["|2","2"]);
        assert_eq!(
            data_rows(&writer.success(
                &mut engine,
                "SELECT pg_stat_reset_shared(),pg_stat_reset_slru(),advisory_result_child(); \
                SELECT last_value FROM advisory_result_numbers",
                true
            )),
            ["||3", "3"]
        );
        drop((writer, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn sql_result_checks_stop_before_later_row_effects_and_finish_all_final_writes() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("sql-result-order-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut writer =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        writer.success(&mut engine,"CREATE SEQUENCE result_numbers; CREATE TABLE result_audit(n bigint); CREATE TABLE result_target(n bigint); \
            CREATE DOMAIN result_positive AS bigint NOT NULL CHECK(VALUE>0); \
            CREATE FUNCTION result_first() RETURNS bigint LANGUAGE SQL AS $$ INSERT INTO result_audit VALUES(0); \
                SELECT nextval('result_numbers') FROM generate_series(1,3) $$; \
            CREATE FUNCTION result_checked() RETURNS SETOF result_positive LANGUAGE SQL AS $$ INSERT INTO result_audit VALUES(1); \
                SELECT nextval('result_numbers')-3 FROM generate_series(1,3) $$; \
            CREATE FUNCTION result_writes() RETURNS bigint LANGUAGE SQL AS $$ \
                INSERT INTO result_target VALUES(nextval('result_numbers')),(nextval('result_numbers')) RETURNING n $$; \
            CREATE FUNCTION result_empty() RETURNS result_positive LANGUAGE SQL AS $$ SELECT NULL::bigint WHERE false $$",true);
        assert_eq!(
            data_rows(&writer.success(
                &mut engine,
                "SELECT result_first(); SELECT last_value FROM result_numbers",
                true
            )),
            ["1", "1"]
        );
        assert_eq!(
            error_sqlstates(&writer.execute(&mut engine, "SELECT * FROM result_checked()", true)),
            ["23514"]
        );
        assert_eq!(data_rows(&writer.success(&mut engine,"SELECT n FROM result_audit ORDER BY n; SELECT last_value FROM result_numbers; \
            SELECT result_writes(); SELECT n FROM result_target ORDER BY n; SELECT result_empty() IS NULL",true)),["0","2","3","3","4","t"]);
        writer.success(
            &mut engine,
            "CREATE FUNCTION result_record(OUT a integer,OUT b integer) LANGUAGE SQL AS $$ \
            INSERT INTO result_audit VALUES(9); SELECT 2147483648::bigint,1 $$",
            true,
        );
        assert_eq!(
            error_sqlstates(&writer.execute(&mut engine, "SELECT result_record()", true)),
            ["22003"]
        );
        assert_eq!(
            data_rows(&writer.success(&mut engine, "SELECT n FROM result_audit", true)),
            ["0"]
        );
        drop((writer, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn sql_table_return_checks_keep_each_completed_row_and_domain_call() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("sql-table-domain-progress-{mode:?}"));
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
        writer.success(&mut engine,"CREATE SEQUENCE table_return_numbers; CREATE TABLE table_return_root(id integer); \
            CREATE TABLE table_return_checks(n bigint); CREATE TABLE table_return_answers(id integer,answer text); \
            CREATE FUNCTION table_return_check(n bigint) RETURNS boolean LANGUAGE plpgsql STRICT AS $$ \
                BEGIN INSERT INTO table_return_checks VALUES(n); PERFORM pg_advisory_xact_lock(205+n); RETURN n>0; END$$; \
            CREATE DOMAIN table_return_domain AS bigint CHECK(table_return_check(VALUE)); \
            CREATE FUNCTION table_return_parent() RETURNS TABLE(n table_return_domain) LANGUAGE SQL AS $$ \
                SELECT nextval('table_return_numbers') FROM generate_series(1,2) $$; \
            CREATE FUNCTION table_return_trigger() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE answer text; \
                BEGIN SELECT string_agg(n::text,',') INTO answer FROM table_return_parent(); \
                INSERT INTO table_return_answers VALUES(NEW.id,answer); RETURN NEW; END$$; \
            CREATE TRIGGER table_return_trigger AFTER INSERT ON table_return_root FOR EACH ROW EXECUTE FUNCTION table_return_trigger()",true);
        holder.success(
            &mut engine,
            "BEGIN; SELECT pg_advisory_lock(206),pg_advisory_lock(207)",
            true,
        );
        let sql = "INSERT INTO table_return_root VALUES(1)";
        let mut status = writer.execute_status(&mut engine, sql, true);
        for (key, issued) in [(206, "1"), (207, "2")] {
            for release in [false, true] {
                assert!(
                    matches!(status, ExecutionStatus::Blocked { .. }),
                    "{mode:?}: {status:?}: {}",
                    String::from_utf8_lossy(writer.send.readable())
                );
                assert_eq!(
                    data_rows(&holder.success(
                        &mut engine,
                        "SELECT last_value FROM table_return_numbers",
                        true
                    )),
                    [issued]
                );
                if release {
                    holder.success(
                        &mut engine,
                        &format!("SELECT pg_advisory_unlock({key})"),
                        true,
                    );
                }
                status = resume_function_progress_wait(&mut engine, &mut writer, sql, status);
            }
        }
        assert_eq!(status, ExecutionStatus::Complete);
        assert!(
            error_sqlstates(writer.send.readable()).is_empty(),
            "{}",
            String::from_utf8_lossy(writer.send.readable())
        );
        holder.success(&mut engine, "COMMIT", true);
        assert_eq!(data_rows(&writer.success(&mut engine,"SELECT n FROM table_return_checks ORDER BY n; SELECT answer FROM table_return_answers; \
            SELECT last_value FROM table_return_numbers",true)),["1","2","1,2","2"]);
        writer.success(&mut engine, "INSERT INTO table_return_root VALUES(2)", true);
        assert_eq!(data_rows(&writer.success(&mut engine,"SELECT n FROM table_return_checks ORDER BY n; SELECT answer FROM table_return_answers ORDER BY id; \
            SELECT last_value FROM table_return_numbers",true)),["1","2","3","4","1,2","3,4","4"]);
        drop((holder, writer, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn sql_row_callbacks_reject_restarts_that_would_hide_their_parent_write() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("sql-row-progress-guard-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut writer =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        writer.success(&mut engine,"CREATE SEQUENCE guarded_numbers; CREATE TABLE guarded_target(n bigint); CREATE TABLE guarded_audit(n bigint); \
            CREATE FUNCTION guarded_child() RETURNS bigint LANGUAGE plpgsql AS $$ \
                BEGIN INSERT INTO guarded_audit SELECT count(*) FROM guarded_target; RETURN 1; END$$; \
            CREATE FUNCTION guarded_parent() RETURNS bigint LANGUAGE SQL AS $$ \
                INSERT INTO guarded_audit VALUES(-1); INSERT INTO guarded_target VALUES(nextval('guarded_numbers')) RETURNING guarded_child() $$",true);
        let error = writer.execute(&mut engine, "SELECT guarded_parent()", true);
        assert_eq!(
            error_sqlstates(&error),
            ["0A000"],
            "{}",
            String::from_utf8_lossy(&error)
        );
        assert!(
            String::from_utf8_lossy(&error)
                .contains("SQL row expression requires retained write progress")
        );
        assert_eq!(
            data_rows(&writer.success(
                &mut engine,
                "SELECT count(*) FROM guarded_target; SELECT count(*) FROM guarded_audit; \
            SELECT last_value FROM guarded_numbers",
                true
            )),
            ["0", "0", "1"]
        );
        writer.success(
            &mut engine,
            "INSERT INTO guarded_target VALUES(nextval('guarded_numbers'))",
            true,
        );
        assert_eq!(
            data_rows(&writer.success(&mut engine, "SELECT n FROM guarded_target", true)),
            ["2"]
        );
        drop((writer, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn sql_result_restarts_reject_unowned_volatile_values_and_keep_owned_arguments() {
    for (label, ctype, value) in [
        ("random", "double precision", "random()"),
        ("clock", "timestamp with time zone", "clock_timestamp()"),
    ] {
        let config = test_config(&format!("sql-volatile-result-{label}"));
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut writer =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        writer.success(&mut engine,&format!("CREATE TABLE nonce_audit(n integer); \
            CREATE FUNCTION nonce_check(p {ctype}) RETURNS boolean LANGUAGE plpgsql STRICT AS $$ BEGIN RETURN true; END$$; \
            CREATE DOMAIN checked_nonce AS {ctype} CHECK(nonce_check(VALUE)); \
            CREATE FUNCTION unowned_nonce() RETURNS SETOF checked_nonce LANGUAGE SQL AS $$ \
                INSERT INTO nonce_audit VALUES(0); SELECT {value} $$; \
            CREATE FUNCTION owned_nonce(p {ctype}) RETURNS checked_nonce LANGUAGE SQL AS $$ \
                INSERT INTO nonce_audit VALUES(1); SELECT p $$"),true);
        let error = writer.execute(&mut engine, "SELECT * FROM unowned_nonce()", true);
        assert_eq!(
            error_sqlstates(&error),
            ["0A000"],
            "{label}: {}",
            String::from_utf8_lossy(&error)
        );
        assert!(
            String::from_utf8_lossy(&error)
                .contains("expression restart requires owned volatile values")
        );
        assert_eq!(
            data_rows(&writer.success(&mut engine, "SELECT count(*) FROM nonce_audit", true)),
            ["0"]
        );
        assert_eq!(
            data_rows(&writer.success(
                &mut engine,
                &format!("SELECT owned_nonce({value}) IS NOT NULL; SELECT n FROM nonce_audit"),
                true
            )),
            ["t", "1"]
        );
        drop((writer, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn sql_function_statements_keep_writes_child_calls_settings_and_selected_source() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for (rows, atomic) in [(false, false), (true, false), (false, true), (true, true)] {
            let mut config =
                test_config(&format!("sql-function-progress-{mode:?}-{rows}-{atomic}"));
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
            writer.success(&mut engine,"SET track_functions='all'; CREATE SEQUENCE sql_numbers; \
                CREATE TABLE sql_root(id integer); CREATE TABLE sql_audit(stage text,n bigint,note text); \
                CREATE FUNCTION sql_child(p text,k integer) RETURNS text LANGUAGE plpgsql AS $$ \
                DECLARE n bigint:=nextval('sql_numbers'); BEGIN INSERT INTO sql_audit VALUES('child',n,p); \
                PERFORM pg_advisory_xact_lock(k); RETURN p||':'||n::text; END$$",true);
            let signature = if rows {
                "RETURNS TABLE(payload text)"
            } else {
                "RETURNS text"
            };
            let application = if !rows && !atomic {
                "body-setting"
            } else {
                "saved-sql-call"
            };
            let setting = if !rows && !atomic {
                "SET LOCAL application_name='body-setting'; "
            } else {
                ""
            };
            let body = format!(
                "{setting}INSERT INTO sql_audit VALUES('prelude',0,p); SELECT sql_child(p,201); \
                INSERT INTO sql_audit VALUES('middle',0,current_setting('application_name')); \
                SELECT sql_child(p,202){}",
                if rows { " UNION ALL SELECT p" } else { "" }
            );
            let source = if atomic {
                format!("BEGIN ATOMIC {body}; END")
            } else {
                format!("AS $${body}$$")
            };
            writer.success(&mut engine,&format!("CREATE FUNCTION sql_parent(p text) {signature} LANGUAGE SQL \
                SECURITY DEFINER SET application_name='saved-sql-call' {source}; \
                CREATE FUNCTION sql_trigger() RETURNS trigger LANGUAGE plpgsql AS $$ \
                DECLARE answer text; BEGIN {}; INSERT INTO sql_audit VALUES('caller',NEW.id,answer); RETURN NEW; END$$; \
                CREATE TRIGGER sql_trigger AFTER INSERT ON sql_root FOR EACH ROW EXECUTE FUNCTION sql_trigger()",
                if rows { "SELECT string_agg(payload,',') INTO answer FROM sql_parent(repeat('x',8192))" } else { "SELECT sql_parent(repeat('x',8192)) INTO answer" }),true);
            holder.success(
                &mut engine,
                "BEGIN; SELECT pg_advisory_lock(201),pg_advisory_lock(202)",
                true,
            );
            let sql = "INSERT INTO sql_root VALUES(1) RETURNING id";
            let mut status = writer.execute_status(&mut engine, sql, true);
            for (key, issued) in [(201, "1"), (202, "2")] {
                for release in [false, true] {
                    assert!(
                        matches!(status, ExecutionStatus::Blocked { .. }),
                        "{mode:?} rows={rows} atomic={atomic}: {status:?}: {}",
                        String::from_utf8_lossy(writer.send.readable())
                    );
                    assert_eq!(
                        data_rows(&holder.success(
                            &mut engine,
                            "SELECT 40+2; SELECT last_value FROM sql_numbers",
                            true
                        )),
                        ["42", issued]
                    );
                    if release {
                        if key == 202 {
                            holder.success(&mut engine,&format!("CREATE OR REPLACE FUNCTION sql_parent(p text) {signature} LANGUAGE SQL SECURITY DEFINER SET application_name='replacement-call' AS $$SELECT 'replacement'$$"),true);
                        }
                        holder.success(
                            &mut engine,
                            &format!("SELECT pg_advisory_unlock({key})"),
                            true,
                        );
                    }
                    status = resume_function_progress_wait(&mut engine, &mut writer, sql, status);
                }
            }
            assert_eq!(status, ExecutionStatus::Complete);
            assert!(
                error_sqlstates(writer.send.readable()).is_empty(),
                "{}",
                String::from_utf8_lossy(writer.send.readable())
            );
            assert_eq!(data_rows(writer.send.readable()), ["1"]);
            assert_eq!(command_tags(writer.send.readable()), ["INSERT 0 1"]);
            holder.success(&mut engine, "COMMIT", true);
            assert_eq!(data_rows(&writer.success(&mut engine,"SELECT stage,n,length(note) FROM sql_audit ORDER BY stage,n; \
                SELECT note FROM sql_audit WHERE stage='middle'; SELECT calls FROM pg_stat_user_functions WHERE funcname='sql_parent'",true)),
                [format!("caller|1|{}",if rows { 16387 } else { 8194 }),"child|1|8192".into(),"child|2|8192".into(),format!("middle|0|{}",application.len()),"prelude|0|8192".into(),application.into(),"1".into()]);
            writer.success(&mut engine, "INSERT INTO sql_root VALUES(2)", true);
            assert_eq!(data_rows(&writer.success(&mut engine,"SELECT count(*) FROM sql_audit; SELECT last_value FROM sql_numbers; \
                SELECT calls FROM pg_stat_user_functions WHERE funcname='sql_parent'; SELECT length(note) FROM sql_audit WHERE stage='caller' AND n=2",true)),["6","2","2","11"]);
            drop((holder, writer, engine));
            std::fs::remove_dir_all(config.data_dir).unwrap();
        }
    }
}

#[test]
fn sql_final_writes_keep_returned_rows_while_after_triggers_wait() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for rows in [false, true] {
            let mut config = test_config(&format!("sql-write-result-progress-{mode:?}-{rows}"));
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
            writer.success(&mut engine,&format!("CREATE SEQUENCE sql_write_numbers; CREATE TABLE sql_write_root(id integer); \
                CREATE TABLE sql_write_target(id integer); CREATE TABLE sql_write_audit(stage text,n bigint); \
                CREATE FUNCTION sql_write_after() RETURNS trigger LANGUAGE plpgsql AS $$ \
                DECLARE n bigint:=nextval('sql_write_numbers'); found_rows bigint; BEGIN \
                SELECT count(*) INTO found_rows FROM sql_write_target; \
                INSERT INTO sql_write_audit VALUES('visible',found_rows); \
                INSERT INTO sql_write_audit VALUES('after',n); PERFORM pg_advisory_xact_lock(203); RETURN NEW; END$$; \
                CREATE TRIGGER sql_write_after AFTER INSERT ON sql_write_target FOR EACH ROW EXECUTE FUNCTION sql_write_after(); \
                CREATE FUNCTION sql_write_parent(p integer) {} LANGUAGE SQL AS $$ \
                INSERT INTO sql_write_audit VALUES('prelude',nextval('sql_write_numbers')); \
                INSERT INTO sql_write_target VALUES(p) RETURNING id $$; \
                CREATE FUNCTION sql_write_trigger() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE answer integer; \
                BEGIN {}; INSERT INTO sql_write_audit VALUES('caller',answer); RETURN NEW; END$$; \
                CREATE TRIGGER sql_write_trigger AFTER INSERT ON sql_write_root FOR EACH ROW EXECUTE FUNCTION sql_write_trigger()",
                if rows { "RETURNS TABLE(id integer)" } else { "RETURNS integer" },
                if rows { "SELECT id INTO answer FROM sql_write_parent(NEW.id)" } else { "SELECT sql_write_parent(NEW.id) INTO answer" }),true);
            holder.success(&mut engine, "BEGIN; SELECT pg_advisory_lock(203)", true);
            let sql = "INSERT INTO sql_write_root VALUES(1)";
            let mut status = writer.execute_status(&mut engine, sql, true);
            for _ in 0..3 {
                assert!(
                    matches!(status, ExecutionStatus::Blocked { .. }),
                    "{mode:?} rows={rows}: {status:?}: {}",
                    String::from_utf8_lossy(writer.send.readable())
                );
                assert_eq!(
                    data_rows(&holder.success(
                        &mut engine,
                        "SELECT last_value FROM sql_write_numbers",
                        true
                    )),
                    ["2"]
                );
                status = resume_function_progress_wait(&mut engine, &mut writer, sql, status);
            }
            holder.success(&mut engine, "SELECT pg_advisory_unlock(203)", true);
            status = resume_function_progress_wait(&mut engine, &mut writer, sql, status);
            assert_eq!(status, ExecutionStatus::Complete);
            assert!(
                error_sqlstates(writer.send.readable()).is_empty(),
                "{}; sequence replay error: {:?}",
                String::from_utf8_lossy(writer.send.readable()),
                sequence::take_replay_error_diagnostic(),
            );
            holder.success(&mut engine, "COMMIT", true);
            assert_eq!(data_rows(&writer.success(&mut engine,"SELECT id FROM sql_write_target; SELECT stage,n FROM sql_write_audit ORDER BY stage; \
                SELECT last_value FROM sql_write_numbers; SELECT currval('sql_write_numbers'),lastval()",true)),["1","after|2","caller|1","prelude|1","visible|1","2","2|2"]);
            writer.success(&mut engine, "INSERT INTO sql_write_root VALUES(2)", true);
            assert_eq!(data_rows(&writer.success(&mut engine,"SELECT count(*) FROM sql_write_target; SELECT count(*) FROM sql_write_audit; SELECT last_value FROM sql_write_numbers",true)),["2","8","4"]);
            drop((holder, writer, engine));
            std::fs::remove_dir_all(config.data_dir).unwrap();
        }
    }
}

#[test]
fn sql_return_checks_resume_after_the_body_without_reissuing_values() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for later in [false, true] {
            let mut config = test_config(&format!("sql-return-check-progress-{mode:?}-{later}"));
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
            let post_wait = if later {
                "PERFORM nextval('sql_return_numbers');"
            } else {
                ""
            };
            writer.success(&mut engine,&format!("CREATE SEQUENCE sql_return_numbers; CREATE TABLE sql_return_root(id integer); CREATE TABLE sql_return_audit(stage text,n bigint); \
            CREATE FUNCTION sql_return_check(n bigint) RETURNS boolean LANGUAGE plpgsql STRICT AS $$ \
            BEGIN INSERT INTO sql_return_audit VALUES('check',n); PERFORM pg_advisory_xact_lock(204); {post_wait} RETURN n>0; END$$; \
            CREATE DOMAIN sql_return_domain AS bigint CHECK(sql_return_check(VALUE)); \
            CREATE FUNCTION sql_return_parent() RETURNS sql_return_domain LANGUAGE SQL AS $$ \
            INSERT INTO sql_return_audit VALUES('body',1); SELECT nextval('sql_return_numbers') $$; \
            CREATE FUNCTION sql_return_trigger() RETURNS trigger LANGUAGE plpgsql AS $$ \
            DECLARE answer bigint; BEGIN SELECT sql_return_parent() INTO answer; INSERT INTO sql_return_audit VALUES('caller',answer); RETURN NEW; END$$; \
            CREATE TRIGGER sql_return_trigger AFTER INSERT ON sql_return_root FOR EACH ROW EXECUTE FUNCTION sql_return_trigger()"),true);
            holder.success(&mut engine, "BEGIN; SELECT pg_advisory_lock(204)", true);
            let sql = "INSERT INTO sql_return_root VALUES(1)";
            let mut status = writer.execute_status(&mut engine, sql, true);
            for _ in 0..3 {
                assert!(
                    matches!(status, ExecutionStatus::Blocked { .. }),
                    "{mode:?}: {status:?}: {}",
                    String::from_utf8_lossy(writer.send.readable())
                );
                assert_eq!(
                    data_rows(&holder.success(
                        &mut engine,
                        "SELECT last_value FROM sql_return_numbers",
                        true
                    )),
                    ["1"]
                );
                status = resume_function_progress_wait(&mut engine, &mut writer, sql, status);
            }
            holder.success(&mut engine, "SELECT pg_advisory_unlock(204)", true);
            status = resume_function_progress_wait(&mut engine, &mut writer, sql, status);
            assert_eq!(status, ExecutionStatus::Complete);
            assert!(
                error_sqlstates(writer.send.readable()).is_empty(),
                "{}; sequence replay error: {:?}",
                String::from_utf8_lossy(writer.send.readable()),
                sequence::take_replay_error_diagnostic(),
            );
            holder.success(&mut engine, "COMMIT", true);
            assert_eq!(data_rows(&writer.success(&mut engine,"SELECT stage,n FROM sql_return_audit ORDER BY stage; SELECT last_value FROM sql_return_numbers; \
            SELECT currval('sql_return_numbers'),lastval()",true)),["body|1","caller|1","check|1",if later { "2" } else { "1" },if later { "2|2" } else { "1|1" }]);
            writer.success(&mut engine, "INSERT INTO sql_return_root VALUES(2)", true);
            assert_eq!(
            data_rows(&writer.success(
                &mut engine,
                "SELECT count(*) FROM sql_return_audit; SELECT last_value FROM sql_return_numbers",
                true
            )),
            ["6", if later { "4" } else { "2" }]
        );
            drop((holder, writer, engine));
            std::fs::remove_dir_all(config.data_dir).unwrap();
        }
    }
}

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
                output_mark.truncate_unsent(&mut writer.send);
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
            output_mark.truncate_unsent(&mut writer.send);
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
                output_mark.truncate_unsent(&mut writer.send);
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
            output_mark.truncate_unsent(&mut writer.send);
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
            output_mark.truncate_unsent(&mut writer.send);
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
        output_mark.truncate_unsent(&mut writer.send);
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
                output_mark.truncate_unsent(&mut writer.send);
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
        for (site, language, body) in [
            (
                "assignment",
                "plpgsql",
                "DECLARE n bigint; BEGIN n:=expression_child(); RETURN n; END",
            ),
            ("return", "plpgsql", "BEGIN RETURN expression_child(); END"),
            (
                "bounds",
                "plpgsql",
                "BEGIN FOR i IN 1..expression_child() LOOP NULL; END LOOP; RETURN 1; END",
            ),
            (
                "message",
                "plpgsql",
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
                CREATE FUNCTION expression_parent() RETURNS bigint LANGUAGE {language} AS $${body}$$; \
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
            let expected = ["2", "2", "2"];
            assert_eq!(data_rows(&writer.success(&mut engine,"SELECT id FROM expression_root; SELECT n FROM expression_audit ORDER BY n; SELECT last_value FROM expression_numbers",true)), expected);
            drop((holder, writer, engine));
            std::fs::remove_dir_all(config.data_dir).unwrap();
        }
    }
}
