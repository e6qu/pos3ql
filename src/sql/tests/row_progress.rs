//! A write keeps the rows it changed while its row expressions call routines
//! that need the engine: lock waits, errors and timeouts inside those calls
//! roll the whole statement back and never repeat an effect.

use super::*;

fn blocked_on_lock(status: ExecutionStatus, writer: &ConfiguredTransactionSession) -> usize {
    let ExecutionStatus::Blocked {
        completed_statements,
        wait,
        ..
    } = status
    else {
        panic!(
            "write did not wait: {status:?}: {}",
            String::from_utf8_lossy(writer.send.readable())
        );
    };
    assert_eq!(wait, StatementWait::Lock);
    completed_statements
}

fn resume(
    engine: &mut Engine,
    writer: &mut ConfiguredTransactionSession,
    sql: &str,
    completed_statements: usize,
) -> ExecutionStatus {
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

const SETUP: &str = "CREATE TABLE items(id int, a bigint); \
    INSERT INTO items VALUES (1, 1), (2, 1), (3, 1); \
    CREATE TABLE calls(id int, seen bigint); CREATE SEQUENCE call_numbers; \
    CREATE FUNCTION step(x int) RETURNS bigint LANGUAGE plpgsql AS $$ \
    DECLARE total bigint; BEGIN \
    IF x = 2 THEN PERFORM pg_advisory_xact_lock(7); END IF; \
    IF x = 3 AND current_setting('application_name') = 'fail' THEN \
    RAISE EXCEPTION 'stop at %', x; END IF; \
    SELECT sum(a) INTO total FROM items; \
    INSERT INTO calls VALUES (x, nextval('call_numbers')); RETURN total; END$$";

/// A call that waits for a lock after earlier rows changed makes the whole
/// statement wait and run again: another client runs meanwhile, and every
/// row and call happens once, with PostgreSQL's values.
#[test]
fn routine_lock_waits_after_kept_rows_restart_the_statement_once() {
    let statements = [
        "UPDATE items SET a = step(id) RETURNING id, a",
        "DO $$BEGIN UPDATE items SET a = step(id); END$$",
    ];
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for sql in statements {
            let mut config = test_config(&format!("row-progress-lock-{mode:?}"));
            config.transaction_mode = mode;
            let mut budget = test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
            let mut holder =
                ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
            writer.success(&mut engine, SETUP, true);
            holder.success(&mut engine, "SELECT pg_advisory_lock(7)", true);
            let completed = blocked_on_lock(writer.execute_status(&mut engine, sql, true), &writer);
            // The waiting statement rolled its rows back; in MVCC mode
            // another client reads the table without waiting.
            if mode == crate::config::TransactionMode::Mvcc {
                assert_eq!(
                    data_rows(&holder.success(
                        &mut engine,
                        "SELECT count(*), sum(a) FROM items; SELECT count(*) FROM calls",
                        true
                    )),
                    ["3|3", "0"],
                    "{sql}"
                );
            }
            holder.success(&mut engine, "SELECT pg_advisory_unlock(7)", true);
            writer.send.clear();
            assert_eq!(
                resume(&mut engine, &mut writer, sql, completed),
                ExecutionStatus::Complete,
                "{mode:?}: {sql}"
            );
            let output = writer.send.readable().to_vec();
            assert!(
                error_sqlstates(&output).is_empty(),
                "{mode:?}: {sql}: {}",
                String::from_utf8_lossy(&output)
            );
            if sql.starts_with("UPDATE") {
                assert_eq!(data_rows(&output), ["1|3", "2|5", "3|9"], "{mode:?}");
            }
            assert_eq!(
                data_rows(&writer.success(
                    &mut engine,
                    "SELECT id, a FROM items ORDER BY id; \
                     SELECT id, seen FROM calls ORDER BY id",
                    true
                )),
                ["1|3", "2|5", "3|9", "1|1", "2|2", "3|3"],
                "{mode:?}: {sql}"
            );
            drop((holder, writer, engine));
            std::fs::remove_dir_all(config.data_dir).unwrap();
        }
    }
}

/// An error in a call after earlier rows changed rolls back the statement,
/// its kept rows and the calls' writes; an exception block catches it.
#[test]
fn routine_errors_after_kept_rows_roll_back_the_statement() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("row-progress-error-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
        writer.success(&mut engine, SETUP, true);
        writer.success(&mut engine, "SET application_name = 'fail'", true);
        let output = writer.execute(
            &mut engine,
            "BEGIN; UPDATE items SET a = step(id) RETURNING id",
            true,
        );
        assert_eq!(error_sqlstates(&output), ["P0001"], "{mode:?}");
        writer.success(&mut engine, "ROLLBACK", true);
        writer.success(
            &mut engine,
            "DO $$BEGIN BEGIN DELETE FROM items WHERE step(id) > 0; \
             EXCEPTION WHEN raise_exception THEN INSERT INTO calls VALUES (0, 0); END; END$$",
            true,
        );
        assert_eq!(
            data_rows(&writer.success(
                &mut engine,
                "SELECT id, a FROM items ORDER BY id; SELECT id, seen FROM calls ORDER BY id",
                true
            )),
            ["1|1", "2|1", "3|1", "0|0"],
            "{mode:?}"
        );
        drop((writer, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}
