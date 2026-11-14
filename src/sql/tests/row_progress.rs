//! A write keeps the rows it changed while its row expressions call routines
//! that need the engine, and while it waits for a lock inside its rows or
//! inside those calls: only the unfinished row rolls back. Errors and
//! timeouts roll the whole statement back; no effect repeats.

use super::*;

/// Asserts a lock wait and discards the output a parked connection would:
/// none when the write keeps its rows.
fn blocked_on_lock(status: ExecutionStatus, writer: &mut ConfiguredTransactionSession) -> usize {
    let ExecutionStatus::Blocked {
        completed_statements,
        wait,
        output_mark,
    } = status
    else {
        panic!(
            "write did not wait: {status:?}: {}",
            String::from_utf8_lossy(writer.send.readable())
        );
    };
    assert_eq!(wait, StatementWait::Lock);
    output_mark.truncate_unsent(&mut writer.send);
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

/// A call that waits for a lock after earlier rows changed makes the
/// statement wait with those rows kept: another client runs meanwhile, and
/// every row and call happens once, with PostgreSQL's values.
#[test]
fn routine_lock_waits_after_kept_rows_resume_once() {
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
            let status = writer.execute_status(&mut engine, sql, true);
            let completed = blocked_on_lock(status, &mut writer);
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
/// up to `write_step_levels` levels (4 by default); a routine call after
/// rows changed one level deeper fails with the named capacity instead of
/// hiding those rows, and a fifth level keeps its progress when the setting
/// allows it. Values from PostgreSQL 18.6.
#[test]
fn nested_step_progress_beyond_its_capacity_fails_loudly() {
    const SETUP: &str = "CREATE TABLE a1(n int); CREATE TABLE a2(n int); CREATE TABLE a3(n int); \
         CREATE TABLE a4(n int); CREATE TABLE a5(n int); CREATE TABLE a6(n int, c bigint); \
         CREATE FUNCTION cnt() RETURNS bigint LANGUAGE plpgsql AS \
           $$BEGIN RETURN (SELECT count(*) FROM a6); END$$; \
         CREATE RULE r1 AS ON INSERT TO a1 DO ALSO INSERT INTO a2 VALUES (new.n); \
         CREATE RULE r2 AS ON INSERT TO a2 DO ALSO INSERT INTO a3 VALUES (new.n); \
         CREATE RULE r3 AS ON INSERT TO a3 DO ALSO INSERT INTO a4 VALUES (new.n); \
         CREATE RULE r4 AS ON INSERT TO a4 DO ALSO INSERT INTO a5 VALUES (new.n); \
         CREATE RULE r5 AS ON INSERT TO a5 DO ALSO \
           INSERT INTO a6 VALUES (new.n, cnt()), (new.n + 1, cnt())";
    for levels in [4, 5] {
        let mut config = test_config(&format!("write-step-levels-{levels}"));
        config.write_step_levels = levels;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
        writer.success(&mut engine, SETUP, true);
        assert_eq!(
            data_rows(&writer.success(
                &mut engine,
                "INSERT INTO a3 VALUES (2); SELECT n, c FROM a6 ORDER BY n, c",
                true
            )),
            ["2|0", "3|1"],
            "{levels}"
        );
        let output = writer.execute(&mut engine, "INSERT INTO a1 VALUES (3)", true);
        if levels == 4 {
            assert_eq!(
                error_sqlstates(&output),
                ["54000"],
                "{}",
                String::from_utf8_lossy(&output)
            );
            assert!(String::from_utf8_lossy(&output).contains(
                "multi-step statements nest deeper than the 4 levels write progress keeps \
                 (write_step_levels)"
            ));
        } else {
            assert!(
                error_sqlstates(&output).is_empty(),
                "{}",
                String::from_utf8_lossy(&output)
            );
            assert_eq!(
                data_rows(&writer.success(&mut engine, "SELECT n, c FROM a6 ORDER BY n, c", true)),
                ["2|0", "3|1", "3|2", "4|3"],
                "{levels}"
            );
        }
        drop((writer, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

/// A call in an `INSERT ... SELECT` output list runs as its row is
/// inserted, so it sees the rows before it; a lock wait inside it keeps the
/// earlier rows, and every row and call happens once.
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
        let status = writer.execute_status(&mut engine, sql, true);
        let completed = blocked_on_lock(status, &mut writer);
        holder.success(&mut engine, "SELECT pg_advisory_unlock(7)", true);
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

/// Holds what makes row `key` of a write in `mode` wait: an uncommitted row
/// with the same unique key, or in locking mode an uncommitted write to the
/// table the row's trigger writes.
fn hold_row(
    engine: &mut Engine,
    holder: &mut ConfiguredTransactionSession,
    mode: crate::config::TransactionMode,
    key: &str,
) {
    let sql = if mode == crate::config::TransactionMode::Locking {
        "BEGIN; INSERT INTO side VALUES (0)".to_string()
    } else {
        format!("BEGIN; INSERT INTO orders VALUES (0, {key}, 'held')")
    };
    holder.success(engine, &sql, true);
}

/// Runs `sql` for `writer` with either protocol, as a parked message runs
/// again; output accumulates in `writer.send`.
fn run_protocol(
    engine: &mut Engine,
    writer: &mut ConfiguredTransactionSession,
    sql: &str,
    extended: bool,
    completed_statements: usize,
    lock_timeout_expired: bool,
) -> ExecutionStatus {
    writer.arena.recycle_front();
    crate::mem::guard::forbid_alloc(|| {
        let mut responder = Responder::new(&mut writer.send);
        if !extended {
            return engine.execute_simple_from(
                sql,
                completed_statements,
                &writer.arena,
                &mut writer.transaction,
                &mut writer.pool,
                &mut writer.cursors,
                &writer.guc,
                &mut responder,
                writer.connection_id,
                lock_timeout_expired,
            );
        }
        engine
            .execute_extended(
                sql,
                &writer.arena,
                &[],
                &[],
                &mut writer.transaction,
                &mut writer.pool,
                &mut writer.cursors,
                &writer.guc,
                &mut responder,
                writer.connection_id,
                lock_timeout_expired,
            )
            .map(|status| match status {
                ExtendedExecutionStatus::Complete(_) => ExecutionStatus::Complete,
                ExtendedExecutionStatus::Blocked { wait } => ExecutionStatus::Blocked {
                    completed_statements: 0,
                    output_mark: responder.mark(),
                    wait,
                },
            })
    })
    .unwrap()
}

const ROW_LOCK_SETUP: &str = "CREATE SEQUENCE numbers; CREATE TABLE audit(stage text, n bigint); \
    CREATE TABLE orders(n bigint DEFAULT nextval('numbers'), id int UNIQUE, note text); \
    CREATE TABLE side(id int); \
    CREATE FUNCTION audit_order() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
    IF NEW.note = 'held' THEN RETURN NEW; END IF; \
    IF NEW.id IN (4, 13, 23) THEN INSERT INTO side VALUES (NEW.id); END IF; \
    INSERT INTO audit VALUES (TG_OP || '-' || NEW.note, nextval('numbers')); \
    NEW.note := NEW.note || '+'; RETURN NEW; END$$; \
    CREATE TRIGGER audit_order BEFORE INSERT OR UPDATE ON orders \
    FOR EACH ROW EXECUTE FUNCTION audit_order()";

/// Lock waits inside a write's rows, several rows in: a unique check waits
/// for another transaction's uncommitted row with the same key, or, in
/// locking mode, a trigger body's write waits for the relation lock of
/// another transaction's uncommitted write. The write
/// keeps its earlier rows while it waits, another client runs, and the
/// write continues at the waiting row once that transaction rolls back:
/// sequence defaults, BEFORE ROW trigger bodies with their own writes and
/// sequence calls, and `RETURNING` rows each happen once, with PostgreSQL
/// 18.6's values; both transaction modes and both protocols.
#[test]
fn row_lock_waits_resume_at_the_waiting_row() {
    let statements: &[(&str, &str, &[&str])] = &[
        (
            "4",
            "INSERT INTO orders(id, note) SELECT g, 'o' || g FROM generate_series(1, 6) g \
             RETURNING n, id, note",
            &[
                "1|1|o1+", "3|2|o2+", "5|3|o3+", "7|4|o4+", "9|5|o5+", "11|6|o6+",
            ],
        ),
        (
            "13",
            "UPDATE orders SET id = id + 10, note = 'u' WHERE id <= 3 RETURNING n, id, note",
            &["1|11|u+", "3|12|u+", "5|13|u+"],
        ),
        (
            "23",
            "INSERT INTO orders(id, note) VALUES (21, 'v1'), (22, 'v2'), (23, 'v3'), \
             (24, 'v4') RETURNING n, id, note",
            &["16|21|v1+", "18|22|v2+", "20|23|v3+", "22|24|v4+"],
        ),
    ];
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for extended in [false, true] {
            let mut config = test_config(&format!("row-lock-waits-{mode:?}-{extended}"));
            config.transaction_mode = mode;
            let mut budget = test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
            let mut holder =
                ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
            writer.success(&mut engine, ROW_LOCK_SETUP, true);
            for &(key, sql, returned) in statements {
                let context = format!("{mode:?} extended={extended}: {sql}");
                hold_row(&mut engine, &mut holder, mode, key);
                writer.send.clear();
                writer.arena.reset();
                let status = run_protocol(&mut engine, &mut writer, sql, extended, 0, false);
                let completed = blocked_on_lock(status, &mut writer);
                assert!(
                    writer.transaction.has_retained_write(),
                    "{context}: the wait keeps the earlier rows"
                );
                // Another client runs while the write waits.
                assert_eq!(
                    data_rows(&holder.success(&mut engine, "SELECT 40 + 2", true)),
                    ["42"],
                    "{context}"
                );
                holder.success(&mut engine, "ROLLBACK", true);
                assert_eq!(
                    run_protocol(&mut engine, &mut writer, sql, extended, completed, false),
                    ExecutionStatus::Complete,
                    "{context}"
                );
                assert!(
                    !writer.transaction.retains_statement_progress(),
                    "{context}"
                );
                let output = writer.send.readable().to_vec();
                assert!(
                    error_sqlstates(&output).is_empty(),
                    "{context}: {}",
                    String::from_utf8_lossy(&output)
                );
                assert_eq!(data_rows(&output), returned, "{context}");
                if extended {
                    // Sync ends the extended protocol's implicit transaction.
                    engine
                        .commit_txn(&mut writer.transaction, &writer.guc)
                        .unwrap();
                }
            }
            assert_eq!(
                data_rows(&writer.success(
                    &mut engine,
                    "SELECT string_agg(stage || ':' || n, ',' ORDER BY n) FROM audit; \
                     SELECT count(*), sum(n), sum(id) FROM orders; \
                     SELECT string_agg(id::text, ',' ORDER BY id) FROM side",
                    true
                )),
                [
                    "INSERT-o1:2,INSERT-o2:4,INSERT-o3:6,INSERT-o4:8,INSERT-o5:10,INSERT-o6:12,\
                     UPDATE-u:13,UPDATE-u:14,UPDATE-u:15,INSERT-v1:17,INSERT-v2:19,INSERT-v3:21,\
                     INSERT-v4:23",
                    "10|112|141",
                    "4,13,23"
                ],
                "{mode:?} extended={extended}"
            );
            drop((holder, writer, engine));
            std::fs::remove_dir_all(config.data_dir).unwrap();
        }
    }
}

/// A lock timeout, an explicit transaction's rollback, or a statement
/// timeout while a write waits inside its rows ends the statement: its kept
/// rows, trigger writes and progress are released, and the next statement
/// starts fresh. Values from PostgreSQL 18.6.
#[test]
fn row_lock_waits_release_kept_rows_on_timeout_and_rollback() {
    let sql = "INSERT INTO orders(id, note) SELECT g, 'o' || g FROM generate_series(1, 6) g \
               RETURNING n";
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for ending in ["lock_timeout", "rollback", "statement_timeout"] {
            let context = format!("{mode:?} {ending}");
            let mut config = test_config(&format!("row-lock-release-{mode:?}-{ending}"));
            config.transaction_mode = mode;
            let mut budget = test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
            let mut holder =
                ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
            writer.success(&mut engine, ROW_LOCK_SETUP, true);
            hold_row(&mut engine, &mut holder, mode, "4");
            match ending {
                "rollback" => {
                    writer.success(&mut engine, "BEGIN", true);
                }
                "statement_timeout" => {
                    writer.success(&mut engine, "SET statement_timeout = 500", true);
                }
                _ => {}
            }
            writer.send.clear();
            writer.arena.reset();
            let status = run_protocol(&mut engine, &mut writer, sql, false, 0, false);
            let completed = blocked_on_lock(status, &mut writer);
            assert!(writer.transaction.has_retained_write(), "{context}");
            if ending == "statement_timeout" {
                std::thread::sleep(std::time::Duration::from_millis(600));
            }
            writer.send.clear();
            assert_eq!(
                run_protocol(
                    &mut engine,
                    &mut writer,
                    sql,
                    false,
                    completed,
                    ending != "statement_timeout"
                ),
                ExecutionStatus::Complete,
                "{context}"
            );
            let expected = if ending == "statement_timeout" {
                "57014"
            } else {
                "55P03"
            };
            assert_eq!(
                error_sqlstates(writer.send.readable()),
                [expected],
                "{context}: {}",
                String::from_utf8_lossy(writer.send.readable())
            );
            if ending == "rollback" {
                writer.success(&mut engine, "ROLLBACK", true);
            }
            assert!(
                !writer.transaction.retains_statement_progress(),
                "{context}"
            );
            holder.success(&mut engine, "ROLLBACK", true);
            writer.success(&mut engine, "RESET statement_timeout", true);
            assert_eq!(
                data_rows(&writer.success(
                    &mut engine,
                    "SELECT count(*) FROM orders; SELECT count(*) FROM audit; \
                     INSERT INTO orders(id, note) VALUES (4, 'after') RETURNING id, note",
                    true
                )),
                ["0", "0", "4|after+"],
                "{context}"
            );
            drop((holder, writer, engine));
            std::fs::remove_dir_all(config.data_dir).unwrap();
        }
    }
}

/// `INSERT ... SELECT` reading its own target selects its source again in
/// each attempt after a routine call: the rows earlier attempts inserted stay
/// invisible to it, as to PostgreSQL's single scan.
#[test]
fn self_reading_insert_select_resumes_with_its_first_source() {
    let config = test_config("row-progress-self-insert");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
    writer.success(
        &mut engine,
        "CREATE TABLE grow (a int); INSERT INTO grow VALUES (1), (2); \
         CREATE FUNCTION same(v int) RETURNS int LANGUAGE plpgsql \
         AS $$BEGIN RETURN v; END$$",
        true,
    );
    let output = writer.success(
        &mut engine,
        "INSERT INTO grow SELECT a + 10 FROM grow WHERE same(a) > 0 RETURNING a, same(a)",
        true,
    );
    assert_eq!(data_rows(&output), ["11|11", "12|12"]);
    assert_eq!(
        data_rows(&writer.success(&mut engine, "SELECT a FROM grow ORDER BY a", true)),
        ["1", "2", "11", "12"]
    );
    drop((writer, engine));
    std::fs::remove_dir_all(config.data_dir).unwrap();
}

const QUALIFY_SETUP: &str = "CREATE TABLE items(id int PRIMARY KEY, a bigint); \
    INSERT INTO items VALUES (1, 1), (2, 1), (3, 1); \
    CREATE TABLE calls(id int, total bigint, n bigint); CREATE SEQUENCE call_numbers; \
    CREATE FUNCTION seen(x int) RETURNS bigint LANGUAGE plpgsql AS $$ \
    DECLARE total bigint; BEGIN \
    IF x = 2 THEN PERFORM pg_advisory_xact_lock(7); END IF; \
    SELECT sum(a) INTO total FROM items; \
    INSERT INTO calls VALUES (x, total, nextval('call_numbers')); \
    RAISE NOTICE 'seen % %', x, total; RETURN total; END$$";

/// A statement with its expected rows, calls and notices.
type QualifyCase<'c> = (&'c str, &'c [&'c str], &'c [&'c str], &'c [&'c str]);

/// `UPDATE` and `DELETE` qualify each row just before changing it, as
/// PostgreSQL does: a condition's call sees the rows already changed, and
/// its notices interleave with the change's own calls. Each call runs once
/// although every call stops the write and resumes it at its row; rows that
/// failed their condition are not qualified again. Values from PostgreSQL
/// 18.6.
#[test]
fn conditions_qualify_each_row_as_it_changes() {
    let update_calls: &[&str] = &["1|3|1", "2|3|2", "-2|3|3", "3|13|4", "-3|13|5"];
    let update_notices: &[&str] = &[
        "seen 1 3",
        "seen 2 3",
        "seen -2 3",
        "seen 3 13",
        "seen -3 13",
    ];
    let cases: &[QualifyCase] = &[
        (
            "UPDATE items SET a = a + 10 + seen(-id) * 0 WHERE seen(id) + id > 4",
            &["1|1", "2|11", "3|11"],
            update_calls,
            update_notices,
        ),
        (
            "DO $$BEGIN UPDATE items SET a = a + 10 + seen(-id) * 0 \
             WHERE seen(id) + id > 4; END$$",
            &["1|1", "2|11", "3|11"],
            update_calls,
            update_notices,
        ),
        (
            "DELETE FROM items WHERE seen(id) > 1 RETURNING id, seen(-id)",
            &["3|1"],
            &["1|3|1", "-1|2|2", "2|2|3", "-2|1|4", "3|1|5"],
            &["seen 1 3", "seen -1 2", "seen 2 2", "seen -2 1", "seen 3 1"],
        ),
    ];
    for &(sql, rows, calls, notices) in cases {
        let config = test_config("row-progress-qualify");
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
        writer.success(&mut engine, QUALIFY_SETUP, true);
        let output = writer.success(&mut engine, sql, true);
        assert_eq!(notice_messages(&output), notices, "{sql}");
        assert_eq!(
            data_rows(&writer.success(
                &mut engine,
                "SELECT id::text || '|' || a FROM items ORDER BY id",
                true
            )),
            rows,
            "{sql}"
        );
        assert_eq!(
            data_rows(&writer.success(
                &mut engine,
                "SELECT id::text || '|' || total || '|' || n FROM calls ORDER BY n",
                true
            )),
            calls,
            "{sql}"
        );
        drop((writer, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

/// A condition's call that waits for a lock after earlier rows changed
/// makes the statement wait with those rows kept; the write resumes at the
/// waiting row without qualifying the earlier rows again, including the
/// row whose condition failed. Values from PostgreSQL 18.6.
#[test]
fn condition_lock_waits_resume_at_the_waiting_row() {
    let statements = [
        "UPDATE items SET a = a + 10 WHERE seen(id) + id > 5",
        "DO $$BEGIN UPDATE items SET a = a + 10 WHERE seen(id) + id > 5; END$$",
    ];
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for sql in statements {
            let context = format!("{mode:?}: {sql}");
            let mut config = test_config(&format!("row-progress-qualify-lock-{mode:?}"));
            config.transaction_mode = mode;
            let mut budget = test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
            let mut holder =
                ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
            writer.success(&mut engine, QUALIFY_SETUP, true);
            writer.success(
                &mut engine,
                "UPDATE items SET a = 2 WHERE id = 3; TRUNCATE calls",
                true,
            );
            holder.success(&mut engine, "SELECT pg_advisory_lock(7)", true);
            let status = writer.execute_status(&mut engine, sql, true);
            let completed = blocked_on_lock(status, &mut writer);
            holder.success(&mut engine, "SELECT pg_advisory_unlock(7)", true);
            assert_eq!(
                resume(&mut engine, &mut writer, sql, completed),
                ExecutionStatus::Complete,
                "{context}"
            );
            let output = writer.send.readable().to_vec();
            assert!(
                error_sqlstates(&output).is_empty(),
                "{context}: {}",
                String::from_utf8_lossy(&output)
            );
            assert_eq!(
                data_rows(&writer.success(
                    &mut engine,
                    "SELECT id::text || '|' || a FROM items ORDER BY id; \
                     SELECT id::text || '|' || total FROM calls ORDER BY n",
                    true
                )),
                ["1|1", "2|11", "3|12", "1|4", "2|4", "3|14"],
                "{context}"
            );
            drop((holder, writer, engine));
            std::fs::remove_dir_all(config.data_dir).unwrap();
        }
    }
}

/// Under read committed, a row another transaction changed while the write
/// waited for its lock is qualified again in its latest version, calling
/// the condition's routine again as PostgreSQL's recheck does; a version
/// that no longer qualifies is skipped. Values from PostgreSQL 18.6.
#[test]
fn condition_rechecks_rows_changed_while_waiting() {
    let config = test_config("row-progress-qualify-recheck");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
    let mut holder = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
    writer.success(
        &mut engine,
        "CREATE TABLE items(id int PRIMARY KEY, a bigint); \
         INSERT INTO items VALUES (1, 1), (2, 1), (3, 1); \
         CREATE FUNCTION small(x int, a bigint) RETURNS boolean LANGUAGE plpgsql AS $$ \
         BEGIN RAISE NOTICE 'small % %', x, a; RETURN a < 50; END$$",
        true,
    );
    holder.success(
        &mut engine,
        "BEGIN; UPDATE items SET a = 100 WHERE id = 2; UPDATE items SET a = 5 WHERE id = 3",
        true,
    );
    let sql = "UPDATE items SET a = a + 10 WHERE small(id, a)";
    writer.send.clear();
    let status = writer.execute_status(&mut engine, sql, true);
    let completed = blocked_on_lock(status, &mut writer);
    holder.success(&mut engine, "COMMIT", true);
    assert_eq!(
        resume(&mut engine, &mut writer, sql, completed),
        ExecutionStatus::Complete
    );
    let output = writer.send.readable().to_vec();
    assert!(
        error_sqlstates(&output).is_empty(),
        "{}",
        String::from_utf8_lossy(&output)
    );
    assert_eq!(
        notice_messages(&output),
        [
            "small 1 1",
            "small 2 1",
            "small 2 100",
            "small 3 1",
            "small 3 5"
        ]
    );
    assert_eq!(
        data_rows(&writer.success(
            &mut engine,
            "SELECT id::text || '|' || a FROM items ORDER BY id",
            true
        )),
        ["1|11", "2|100", "3|15"]
    );
    drop((holder, writer, engine));
    std::fs::remove_dir_all(config.data_dir).unwrap();
}
