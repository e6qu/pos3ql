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
        "MERGE INTO items t USING (SELECT id FROM items) s ON t.id = s.id \
         WHEN MATCHED THEN UPDATE SET a = step(s.id) RETURNING t.id, t.a",
        "DO $$BEGIN MERGE INTO items t USING (SELECT id FROM items) s ON t.id = s.id \
         WHEN MATCHED THEN UPDATE SET a = step(s.id); END$$",
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
            if sql.contains("RETURNING") {
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

/// Dropping a schema whose view has an INSTEAD OF trigger commits the
/// trigger's own drop after the view's.
#[test]
fn schema_drop_commits_view_trigger_once() {
    let config = test_config("drop-schema-view-trigger");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
    writer.success(
        &mut engine,
        "CREATE SCHEMA sp; SET search_path = sp, public; CREATE TABLE items(id int); \
         CREATE VIEW v AS SELECT id FROM items; \
         CREATE FUNCTION v_write() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN new; END$$; \
         CREATE TRIGGER v_write INSTEAD OF INSERT ON v FOR EACH ROW EXECUTE FUNCTION v_write()",
        true,
    );
    writer.success(&mut engine, "DROP SCHEMA sp CASCADE", true);
    assert_eq!(
        data_rows(&writer.success(&mut engine, "SELECT count(*) FROM pg_trigger", true)),
        ["0"]
    );
    drop((writer, engine));
    std::fs::remove_dir_all(config.data_dir).unwrap();
}

/// Multi-step statements nested in one another's steps keep their progress
/// up to four levels; a routine call after rows changed one level deeper
/// fails with the named capacity instead of hiding those rows.
#[test]
fn nested_step_progress_beyond_its_capacity_fails_loudly() {
    let (mut engine, mut budget) = test_engine();
    let output = run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE a1(n int); CREATE TABLE a2(n int); CREATE TABLE a3(n int); \
         CREATE TABLE a4(n int); CREATE TABLE a5(n int); CREATE TABLE a6(n int, c bigint); \
         CREATE FUNCTION cnt() RETURNS bigint LANGUAGE plpgsql AS \
           $$BEGIN RETURN (SELECT count(*) FROM a6); END$$; \
         CREATE RULE r1 AS ON INSERT TO a1 DO ALSO INSERT INTO a2 VALUES (new.n); \
         CREATE RULE r2 AS ON INSERT TO a2 DO ALSO INSERT INTO a3 VALUES (new.n); \
         CREATE RULE r3 AS ON INSERT TO a3 DO ALSO INSERT INTO a4 VALUES (new.n); \
         CREATE RULE r4 AS ON INSERT TO a4 DO ALSO INSERT INTO a5 VALUES (new.n); \
         CREATE RULE r5 AS ON INSERT TO a5 DO ALSO \
           INSERT INTO a6 VALUES (new.n, cnt()), (new.n + 1, cnt()); \
         INSERT INTO a3 VALUES (2); SELECT n, c FROM a6 ORDER BY n, c",
    );
    assert!(
        error_sqlstates(&output).is_empty(),
        "{}",
        String::from_utf8_lossy(&output)
    );
    assert_eq!(data_rows(&output), ["2|0", "3|1"]);
    let output = run_with(&mut engine, &mut budget, "INSERT INTO a1 VALUES (3)");
    assert_eq!(
        error_sqlstates(&output),
        ["54000"],
        "{}",
        String::from_utf8_lossy(&output)
    );
    assert!(
        String::from_utf8_lossy(&output)
            .contains("multi-step statements nest deeper than the 4 levels write progress keeps")
    );
}

/// A call in an `INSERT ... SELECT` output list runs as its row is
/// inserted, so it sees the rows before it; a lock wait inside it restarts
/// the statement, and every row and call happens once.
#[test]
fn insert_select_output_calls_see_earlier_rows_across_lock_waits() {
    let sql =
        "INSERT INTO items SELECT g + 10, step(g) FROM generate_series(1, 3) g RETURNING id, a";
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("insert-source-calls-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
        let mut holder =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(&mut engine, SETUP, true);
        holder.success(&mut engine, "SELECT pg_advisory_lock(7)", true);
        let completed = blocked_on_lock(writer.execute_status(&mut engine, sql, true), &writer);
        holder.success(&mut engine, "SELECT pg_advisory_unlock(7)", true);
        writer.send.clear();
        assert_eq!(
            resume(&mut engine, &mut writer, sql, completed),
            ExecutionStatus::Complete,
            "{mode:?}"
        );
        let output = writer.send.readable().to_vec();
        assert!(
            error_sqlstates(&output).is_empty(),
            "{mode:?}: {}",
            String::from_utf8_lossy(&output)
        );
        assert_eq!(data_rows(&output), ["11|3", "12|6", "13|12"], "{mode:?}");
        assert_eq!(
            data_rows(&writer.success(
                &mut engine,
                "SELECT id, a FROM items WHERE id > 10 ORDER BY id; \
                 SELECT id, seen FROM calls ORDER BY id",
                true
            )),
            ["11|3", "12|6", "13|12", "1|1", "2|2", "3|3"],
            "{mode:?}"
        );
        drop((holder, writer, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}
