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
