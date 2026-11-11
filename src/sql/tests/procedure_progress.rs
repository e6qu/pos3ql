use super::*;

fn resume_procedure_wait(
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
            "procedure stopped waiting: {status:?}: {}",
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

/// A procedure called from PL/pgSQL that waits for a lock finishes once its
/// statement resumes: inserts, sequence values and INOUT results happen once,
/// from a top-level `CALL` and from a `DO` block.
#[test]
fn nested_procedure_waits_keep_effects_and_outputs_once() {
    for (label, sql, tag) in [
        ("call", "CALL outer_proc()", "CALL"),
        ("do", "DO $$BEGIN CALL outer_proc(); END$$", "DO"),
    ] {
        for mode in [
            crate::config::TransactionMode::Mvcc,
            crate::config::TransactionMode::Locking,
        ] {
            let mut config = test_config(&format!("nested-procedure-wait-{label}-{mode:?}"));
            config.transaction_mode = mode;
            let mut budget = test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut writer =
                ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
            let mut holder =
                ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
            writer.success(
                &mut engine,
                "SET track_functions = 'all'; \
                 CREATE SEQUENCE proc_numbers; CREATE TABLE proc_log(stage text, n bigint); \
                 CREATE PROCEDURE inner_proc(INOUT v int) LANGUAGE plpgsql AS $$ \
                 BEGIN INSERT INTO proc_log VALUES('inner-before', nextval('proc_numbers')); \
                 PERFORM pg_advisory_xact_lock(511); \
                 INSERT INTO proc_log VALUES('inner-after', nextval('proc_numbers')); \
                 v := v + 1; END$$; \
                 CREATE PROCEDURE outer_proc() LANGUAGE plpgsql AS $$ \
                 DECLARE v int := 10; BEGIN \
                 INSERT INTO proc_log VALUES('outer-before', nextval('proc_numbers')); \
                 CALL inner_proc(v); \
                 INSERT INTO proc_log VALUES('outer-after-' || v, nextval('proc_numbers')); END$$",
                true,
            );
            holder.success(&mut engine, "BEGIN; SELECT pg_advisory_lock(511)", true);
            let mut status = writer.execute_status(&mut engine, sql, true);
            assert!(
                matches!(status, ExecutionStatus::Blocked { .. }),
                "{label}: {status:?}"
            );
            assert_eq!(
                data_rows(&holder.success(&mut engine, "SELECT 40 + 2", true)),
                ["42"]
            );
            holder.success(&mut engine, "SELECT pg_advisory_unlock(511); COMMIT", true);
            status = resume_procedure_wait(&mut engine, &mut writer, sql, status);
            assert_eq!(status, ExecutionStatus::Complete, "{label}");
            assert!(
                error_sqlstates(writer.send.readable()).is_empty(),
                "{label}: {}",
                String::from_utf8_lossy(writer.send.readable())
            );
            assert_eq!(command_tags(writer.send.readable()), [tag], "{label}");
            assert_eq!(
                data_rows(&writer.success(
                    &mut engine,
                    "SELECT stage || ':' || n FROM proc_log ORDER BY n; \
                     SELECT funcname || ':' || calls FROM pg_stat_user_functions \
                     ORDER BY funcname",
                    true
                )),
                [
                    "outer-before:1",
                    "inner-before:2",
                    "inner-after:3",
                    "outer-after-11:4",
                    "inner_proc:1",
                    "outer_proc:1",
                ],
                "{label}"
            );
            drop((holder, writer, engine));
            std::fs::remove_dir_all(config.data_dir).unwrap();
        }
    }
}
