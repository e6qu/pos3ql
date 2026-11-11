//! A row trigger fired inside a write whose body needs the statement
//! executor (DDL, writes to views, procedures with DDL) runs once on it,
//! between write attempts, and the write takes its saved result. Expected
//! values come from PostgreSQL 18.6.

use super::*;

const SETUP: &str = "CREATE SEQUENCE s; \
    CREATE TABLE t(id int PRIMARY KEY, v text, n bigint); \
    CREATE TABLE log(msg text); CREATE VIEW logv AS SELECT msg FROM log; \
    CREATE FUNCTION bt() RETURNS trigger LANGUAGE plpgsql AS $$ \
    DECLARE k bigint; BEGIN \
      k := nextval('s'); RAISE NOTICE 'bt % %', TG_OP, k; \
      IF TG_OP = 'DELETE' THEN \
        INSERT INTO logv VALUES ('del ' || OLD.id || ' ' || k); \
        IF OLD.id = 2 THEN RETURN NULL; END IF; RETURN OLD; \
      END IF; \
      EXECUTE 'CREATE TABLE IF NOT EXISTS made_' || NEW.id || '(x int)'; \
      INSERT INTO logv VALUES (TG_OP || ' ' || NEW.id || ' ' || k || ' ' || \
        (SELECT count(*) FROM t)); \
      IF NEW.id = 3 THEN RETURN NULL; END IF; \
      IF NEW.id = 2 AND current_setting('application_name') = 'wait' THEN \
        PERFORM pg_advisory_xact_lock(7); END IF; \
      NEW.n := k; NEW.v := upper(NEW.v); RETURN NEW; \
    END $$; \
    CREATE TRIGGER bt BEFORE INSERT OR UPDATE OR DELETE ON t \
      FOR EACH ROW EXECUTE FUNCTION bt()";

const CHECK: &str = "SELECT id, v, n FROM t ORDER BY id; SELECT msg FROM log ORDER BY msg; \
    SELECT last_value FROM s; SELECT count(*) FROM pg_class WHERE relname LIKE 'made_%'";

fn notice_messages(bytes: &[u8]) -> Vec<String> {
    let mut messages = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let length = u32::from_be_bytes(bytes[offset + 1..offset + 5].try_into().unwrap()) as usize;
        if bytes[offset] == b'N'
            && let Some(message) = bytes[offset + 5..offset + 1 + length]
                .split(|byte| *byte == 0)
                .find(|field| field.first() == Some(&b'M'))
        {
            messages.push(String::from_utf8(message[1..].to_vec()).unwrap());
        }
        offset += 1 + length;
    }
    messages
}

fn run_extended(
    engine: &mut Engine,
    session: &mut ConfiguredTransactionSession,
    sql: &str,
) -> ExtendedExecutionStatus {
    session.send.clear();
    session.arena.reset();
    let mut responder = Responder::new(&mut session.send);
    crate::mem::guard::forbid_alloc(|| {
        engine.execute_extended(
            sql,
            &session.arena,
            &[],
            &[],
            &mut session.transaction,
            &mut session.pool,
            &mut session.cursors,
            &session.guc,
            &mut responder,
            session.connection_id,
            false,
        )
    })
    .unwrap()
}

/// DDL, a view write and a sequence call in a BEFORE row trigger run once
/// per firing, in both transaction modes and both protocols, with
/// PostgreSQL's returned rows, skipped rows, notices and effects.
#[test]
fn before_triggers_needing_the_executor_run_once() {
    let statements: [(&str, &[&str], &[&str]); 3] = [
        (
            "INSERT INTO t(id, v) VALUES (1,'a'),(2,'b'),(3,'c') RETURNING id, v, n",
            &["1|A|1", "2|B|2"],
            &["bt INSERT 1", "bt INSERT 2", "bt INSERT 3"],
        ),
        (
            "UPDATE t SET v = v || '!' WHERE id = 1 RETURNING id, v, n",
            &["1|A!|4"],
            &[
                "bt UPDATE 4",
                "relation \"made_1\" already exists, skipping",
            ],
        ),
        (
            "DELETE FROM t WHERE id IN (1,2) RETURNING id",
            &["1"],
            &["bt DELETE 5", "bt DELETE 6"],
        ),
    ];
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for extended in [false, true] {
            let mut config = test_config(&format!("engine-triggers-{mode:?}-{extended}"));
            config.transaction_mode = mode;
            let mut budget = test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
            writer.success(&mut engine, SETUP, true);
            for (sql, rows, notices) in statements {
                let output = if extended {
                    assert!(matches!(
                        run_extended(&mut engine, &mut writer, sql),
                        ExtendedExecutionStatus::Complete(_)
                    ));
                    writer.send.readable().to_vec()
                } else {
                    writer.execute(&mut engine, sql, true)
                };
                let context = format!("{mode:?} extended={extended}: {sql}");
                assert!(
                    error_sqlstates(&output).is_empty(),
                    "{context}: {}",
                    String::from_utf8_lossy(&output)
                );
                assert_eq!(data_rows(&output), rows, "{context}");
                assert_eq!(notice_messages(&output), notices, "{context}");
            }
            assert_eq!(
                data_rows(&writer.success(&mut engine, CHECK, true)),
                [
                    "2|B|2",
                    "INSERT 1 1 0",
                    "INSERT 2 2 1",
                    "INSERT 3 3 2",
                    "UPDATE 1 4 2",
                    "del 1 5",
                    "del 2 6",
                    "6",
                    "3"
                ],
                "{mode:?} extended={extended}"
            );
            drop((writer, engine));
            std::fs::remove_dir_all(config.data_dir).unwrap();
        }
    }
}

/// A lock wait inside a trigger body running on the executor rolls the
/// whole statement back, DDL included; another client runs meanwhile, and
/// the statement then runs once with the same sequence values.
#[test]
fn executor_trigger_lock_waits_restart_the_statement_once() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("engine-trigger-wait-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
        let mut holder =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(&mut engine, SETUP, true);
        writer.success(&mut engine, "SET application_name = 'wait'", true);
        holder.success(&mut engine, "SELECT pg_advisory_lock(7)", true);
        let sql = "INSERT INTO t(id, v) VALUES (1,'a'),(2,'b'),(3,'c') RETURNING id, v, n";
        let ExecutionStatus::Blocked {
            completed_statements,
            wait,
            ..
        } = writer.execute_status(&mut engine, sql, true)
        else {
            panic!("{mode:?}: the trigger did not wait");
        };
        assert_eq!(wait, StatementWait::Lock);
        // The waiting statement rolled back its rows and the trigger's DDL.
        assert_eq!(
            data_rows(&holder.success(
                &mut engine,
                "SELECT count(*) FROM pg_class WHERE relname LIKE 'made_%'",
                true
            )),
            ["0"],
            "{mode:?}"
        );
        if mode == crate::config::TransactionMode::Mvcc {
            assert_eq!(
                data_rows(&holder.success(&mut engine, "SELECT count(*) FROM log", true)),
                ["0"]
            );
        }
        holder.success(&mut engine, "SELECT pg_advisory_unlock(7)", true);
        writer.send.clear();
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
        assert_eq!(status, ExecutionStatus::Complete, "{mode:?}");
        let output = writer.send.readable().to_vec();
        assert!(
            error_sqlstates(&output).is_empty(),
            "{mode:?}: {}",
            String::from_utf8_lossy(&output)
        );
        assert_eq!(data_rows(&output), ["1|A|1", "2|B|2"], "{mode:?}");
        assert_eq!(
            notice_messages(&output),
            ["bt INSERT 1", "bt INSERT 2", "bt INSERT 3"],
            "{mode:?}"
        );
        assert_eq!(
            data_rows(&writer.success(&mut engine, CHECK, true)),
            [
                "1|A|1",
                "2|B|2",
                "INSERT 1 1 0",
                "INSERT 2 2 1",
                "INSERT 3 3 2",
                "3",
                "3"
            ],
            "{mode:?}"
        );
        drop((holder, writer, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

/// An error in a trigger body running on the executor, a statement timeout
/// while it waits, and a rollback all undo the statement and the body's
/// DDL and view writes.
#[test]
fn executor_trigger_errors_timeouts_and_rollbacks_undo_its_effects() {
    let config = test_config("engine-trigger-errors");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
    let mut holder = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
    writer.success(&mut engine, SETUP, true);
    writer.success(
        &mut engine,
        "CREATE FUNCTION fails() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN \
         INSERT INTO logv VALUES ('fails ' || NEW.id); \
         IF NEW.id = 12 THEN RAISE EXCEPTION 'stop at %', NEW.id; END IF; RETURN NEW; END$$; \
         CREATE TABLE e(id int); \
         CREATE TRIGGER fails BEFORE INSERT ON e FOR EACH ROW EXECUTE FUNCTION fails()",
        true,
    );
    let output = writer.execute(&mut engine, "INSERT INTO e VALUES (11), (12)", true);
    assert_eq!(error_sqlstates(&output), ["P0001"]);
    let output = writer.execute(
        &mut engine,
        "BEGIN; INSERT INTO t(id, v) VALUES (1, 'a'); ROLLBACK",
        true,
    );
    assert!(error_sqlstates(&output).is_empty());
    writer.success(
        &mut engine,
        "SET application_name = 'wait'; SET statement_timeout = '50ms'",
        true,
    );
    holder.success(&mut engine, "SELECT pg_advisory_lock(7)", true);
    let sql = "INSERT INTO t(id, v) VALUES (1,'a'),(2,'b')";
    let ExecutionStatus::Blocked {
        completed_statements,
        ..
    } = writer.execute_status(&mut engine, sql, true)
    else {
        panic!("the trigger did not wait");
    };
    std::thread::sleep(std::time::Duration::from_millis(80));
    writer.send.clear();
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
    assert_eq!(error_sqlstates(writer.send.readable()), ["57014"]);
    holder.success(&mut engine, "SELECT pg_advisory_unlock(7)", true);
    writer.success(&mut engine, "RESET statement_timeout", true);
    assert_eq!(
        data_rows(&writer.success(
            &mut engine,
            "SELECT count(*) FROM e; SELECT count(*) FROM t; SELECT count(*) FROM log; \
             SELECT count(*) FROM pg_class WHERE relname LIKE 'made_%'",
            true
        )),
        ["0", "0", "0", "0"]
    );
    drop((holder, writer, engine));
    std::fs::remove_dir_all(config.data_dir).unwrap();
}

/// A `FOR` loop over a data change whose AFTER trigger waits for a lock
/// keeps the change's returned rows with the waiting body, lets another
/// client run, and then loops over them once, static and through `EXECUTE`.
#[test]
fn for_loops_over_data_changes_wait_for_after_triggers() {
    let setup = "CREATE SEQUENCE loop_numbers; \
        CREATE TABLE loop_rows(id int, n bigint DEFAULT nextval('loop_numbers')); \
        CREATE TABLE loop_audit(msg text); \
        CREATE FUNCTION loop_after() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN \
          PERFORM pg_advisory_xact_lock(7); \
          INSERT INTO loop_audit VALUES ('after ' || NEW.id || ' ' || NEW.n); RETURN NULL; END$$; \
        CREATE TRIGGER loop_after AFTER INSERT ON loop_rows \
          FOR EACH ROW EXECUTE FUNCTION loop_after()";
    let tail = "s := s + r.n; RAISE NOTICE 'row % %', r.id, r.n; END LOOP; \
        FOR r IN EXECUTE 'INSERT INTO loop_rows(id) SELECT 3 WHERE false RETURNING *' LOOP \
          RAISE NOTICE 'never'; END LOOP; \
        RAISE NOTICE 'sum % found % r %', s, found, r; END$$";
    for source in [
        "INSERT INTO loop_rows(id) VALUES (1), (2) RETURNING *",
        "EXECUTE 'INSERT INTO loop_rows(id) VALUES ($1), ($2) RETURNING *' USING 1, 2",
    ] {
        for mode in [
            crate::config::TransactionMode::Mvcc,
            crate::config::TransactionMode::Locking,
        ] {
            let mut config = test_config(&format!("for-dml-after-wait-{mode:?}"));
            config.transaction_mode = mode;
            let mut budget = test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
            let mut holder =
                ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
            writer.success(&mut engine, setup, true);
            holder.success(&mut engine, "SELECT pg_advisory_lock(7)", true);
            let sql = format!(
                "DO $$DECLARE r record; s bigint := 0; BEGIN FOR r IN {source} LOOP {tail}"
            );
            let mut status = writer.execute_status(&mut engine, &sql, true);
            let mut waits = 0;
            while let ExecutionStatus::Blocked {
                completed_statements,
                wait,
                ..
            } = status
            {
                assert_eq!(wait, StatementWait::Lock, "{mode:?}: {source}");
                if waits == 0 {
                    assert_eq!(
                        data_rows(&holder.success(&mut engine, "SELECT 40 + 2", true)),
                        ["42"]
                    );
                    holder.success(&mut engine, "SELECT pg_advisory_unlock(7)", true);
                }
                waits += 1;
                assert!(waits < 8, "{mode:?}: {source}: the loop kept waiting");
                writer.send.clear();
                writer.arena.recycle_front();
                status = crate::mem::guard::forbid_alloc(|| {
                    engine.execute_simple_from(
                        &sql,
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
            assert!(waits > 0, "{mode:?}: {source}: the AFTER trigger must wait");
            let output = writer.send.readable().to_vec();
            assert!(
                error_sqlstates(&output).is_empty(),
                "{mode:?}: {source}: {}",
                String::from_utf8_lossy(&output)
            );
            assert_eq!(
                notice_messages(&output),
                ["row 1 1", "row 2 2", "sum 3 found f r (,)"],
                "{mode:?}: {source}"
            );
            assert_eq!(
                data_rows(&writer.success(
                    &mut engine,
                    "SELECT id, n FROM loop_rows ORDER BY id; \
                     SELECT msg FROM loop_audit ORDER BY msg",
                    true
                )),
                ["1|1", "2|2", "after 1 1", "after 2 2"],
                "{mode:?}: {source}"
            );
            drop((holder, writer, engine));
            std::fs::remove_dir_all(config.data_dir).unwrap();
        }
    }
}

const QUERY_FUNCTION_SETUP: &str = "CREATE SEQUENCE q; CREATE TABLE qt(a int); \
    CREATE TABLE qlog(m text, k bigint); \
    CREATE FUNCTION note(x text) RETURNS int LANGUAGE plpgsql AS $$ \
    BEGIN INSERT INTO qlog VALUES (x, nextval('q')); RETURN 1; END$$; \
    CREATE FUNCTION aft() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
      IF current_setting('application_name') = 'wait' THEN \
        PERFORM pg_advisory_xact_lock(7); END IF; \
      PERFORM note('after ' || NEW.a || ' ' || (SELECT count(*) FROM qt)); \
      EXECUTE 'CREATE TABLE IF NOT EXISTS qmade_' || NEW.a || '(x int)'; \
      RETURN NULL; END$$; \
    CREATE TRIGGER ta AFTER INSERT ON qt FOR EACH ROW EXECUTE FUNCTION aft(); \
    CREATE FUNCTION w(n int) RETURNS bigint LANGUAGE plpgsql AS $$ \
    BEGIN INSERT INTO qt VALUES (n), (n + 1); RETURN (SELECT count(*) FROM qlog); END$$; \
    CREATE FUNCTION s(n int) RETURNS bigint LANGUAGE sql AS $$ \
    INSERT INTO qt VALUES (n); SELECT count(*) FROM qlog $$";

/// A function called from a query takes its writes' AFTER triggers, which
/// run on the executor: their routine calls and DDL work as in PostgreSQL.
#[test]
fn query_function_after_triggers_run_on_the_executor() {
    let (mut engine, mut budget) = test_engine();
    let output = run_with(
        &mut engine,
        &mut budget,
        &format!(
            "{QUERY_FUNCTION_SETUP}; SELECT w(1); SELECT w(10) FROM generate_series(1, 2); \
             SELECT s(20); SELECT m, k FROM qlog ORDER BY k; \
             SELECT relname FROM pg_class WHERE relname LIKE 'qmade_%' ORDER BY relname"
        ),
    );
    assert!(
        error_sqlstates(&output).is_empty(),
        "{}",
        String::from_utf8_lossy(&output)
    );
    assert_eq!(
        data_rows(&output),
        [
            "2",
            "4",
            "6",
            "7",
            "after 1 2|1",
            "after 2 2|2",
            "after 10 4|3",
            "after 11 4|4",
            "after 10 6|5",
            "after 11 6|6",
            "after 20 7|7",
            "qmade_1",
            "qmade_10",
            "qmade_11",
            "qmade_2",
            "qmade_20"
        ]
    );
}

/// A lock wait in such a trigger restarts the statement: another client
/// runs meanwhile, and every write and sequence value happens once.
#[test]
fn query_function_after_trigger_lock_waits_restart_the_statement_once() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for sql in ["SELECT w(1)", "SELECT s(1)"] {
            let mut config = test_config(&format!("query-function-after-wait-{mode:?}"));
            config.transaction_mode = mode;
            let mut budget = test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
            let mut holder =
                ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
            writer.success(&mut engine, QUERY_FUNCTION_SETUP, true);
            writer.success(&mut engine, "SET application_name = 'wait'", true);
            holder.success(&mut engine, "SELECT pg_advisory_lock(7)", true);
            let ExecutionStatus::Blocked {
                completed_statements,
                wait,
                ..
            } = writer.execute_status(&mut engine, sql, true)
            else {
                panic!(
                    "{mode:?}: {sql}: the trigger did not wait: {}",
                    String::from_utf8_lossy(writer.send.readable())
                );
            };
            assert_eq!(wait, StatementWait::Lock);
            assert_eq!(
                data_rows(&holder.success(
                    &mut engine,
                    "SELECT count(*) FROM pg_class WHERE relname LIKE 'qmade_%'",
                    true
                )),
                ["0"],
                "{mode:?}: {sql}"
            );
            holder.success(&mut engine, "SELECT pg_advisory_unlock(7)", true);
            writer.send.clear();
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
            assert_eq!(status, ExecutionStatus::Complete, "{mode:?}: {sql}");
            let output = writer.send.readable().to_vec();
            assert!(
                error_sqlstates(&output).is_empty(),
                "{mode:?}: {sql}: {}",
                String::from_utf8_lossy(&output)
            );
            let (result, log): (&[&str], &[&str]) = if sql == "SELECT w(1)" {
                (&["2"], &["after 1 2|1", "after 2 2|2", "2"])
            } else {
                (&["1"], &["after 1 1|1", "1"])
            };
            assert_eq!(data_rows(&output), result, "{mode:?}: {sql}");
            assert_eq!(
                data_rows(&writer.success(
                    &mut engine,
                    "SELECT m, k FROM qlog ORDER BY k; \
                     SELECT count(*) FROM pg_class WHERE relname LIKE 'qmade_%'",
                    true
                )),
                log,
                "{mode:?}: {sql}"
            );
            drop((holder, writer, engine));
            std::fs::remove_dir_all(config.data_dir).unwrap();
        }
    }
}
