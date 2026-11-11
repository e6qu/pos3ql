use super::object_reads::{advance, read_request, replace_uncached_blocks, reply};
use super::*;
use std::net::TcpListener;
use std::time::{Duration, Instant};

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

fn blocked_wait(status: &ExecutionStatus, writer: &ConfiguredTransactionSession) -> StatementWait {
    match status {
        ExecutionStatus::Blocked { wait, .. } => *wait,
        other => panic!(
            "statement did not wait: {other:?}: {}",
            String::from_utf8_lossy(writer.send.readable())
        ),
    }
}

/// A procedure resumes at its waiting statement, also after its own `COMMIT`
/// and `ROLLBACK`: a row-lock wait in an `UPDATE` after `COMMIT`, then a
/// nested `CALL` whose callee commits and waits for an advisory lock. Each
/// insert, sequence value, INOUT result and statistics count happens once, as
/// in PostgreSQL 18.6, from `CALL` and from `DO`, in both transaction modes.
#[test]
fn procedures_resume_waits_after_transaction_control() {
    for (label, sql, tag) in [
        ("call", "CALL wait_outer()", "CALL"),
        ("do", "DO $$BEGIN CALL wait_outer(); END$$", "DO"),
    ] {
        for mode in [
            crate::config::TransactionMode::Mvcc,
            crate::config::TransactionMode::Locking,
        ] {
            let mut config = test_config(&format!("procedure-resume-{label}-{mode:?}"));
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
                 CREATE SEQUENCE wait_numbers; CREATE TABLE wait_log(stage text, n bigint); \
                 CREATE TABLE wait_rows(id int PRIMARY KEY, v int); \
                 INSERT INTO wait_rows VALUES (1, 0); \
                 CREATE PROCEDURE wait_inner(INOUT v int) LANGUAGE plpgsql AS $$ BEGIN \
                 INSERT INTO wait_log VALUES ('inner-before', nextval('wait_numbers')); \
                 COMMIT; PERFORM pg_advisory_xact_lock(611); \
                 INSERT INTO wait_log VALUES ('inner-after', nextval('wait_numbers')); \
                 v := v + 1; END$$; \
                 CREATE PROCEDURE wait_outer() LANGUAGE plpgsql AS $$ \
                 DECLARE n int := 10; BEGIN \
                 INSERT INTO wait_log VALUES ('outer-before', nextval('wait_numbers')); \
                 COMMIT; UPDATE wait_rows SET v = v + 1 WHERE id = 1; \
                 INSERT INTO wait_log VALUES ('outer-updated', nextval('wait_numbers')); \
                 ROLLBACK; CALL wait_inner(n); \
                 INSERT INTO wait_log VALUES ('outer-after-' || n, nextval('wait_numbers')); \
                 END$$",
                true,
            );
            holder.success(
                &mut engine,
                "BEGIN; UPDATE wait_rows SET v = 100 WHERE id = 1; SELECT pg_advisory_lock(611)",
                true,
            );
            let mut status = writer.execute_status(&mut engine, sql, true);
            assert_eq!(
                blocked_wait(&status, &writer),
                StatementWait::Lock,
                "{label}"
            );
            // The first transaction committed before the wait.
            assert_eq!(
                data_rows(&holder.success(
                    &mut engine,
                    "SELECT stage || ':' || n FROM wait_log ORDER BY n",
                    true
                )),
                ["outer-before:1"],
                "{label}"
            );
            holder.success(&mut engine, "COMMIT", true);
            status = resume_procedure_wait(&mut engine, &mut writer, sql, status);
            assert_eq!(
                blocked_wait(&status, &writer),
                StatementWait::Lock,
                "{label}"
            );
            assert_eq!(
                data_rows(&holder.success(
                    &mut engine,
                    "SELECT stage || ':' || n FROM wait_log ORDER BY n",
                    true
                )),
                ["outer-before:1", "inner-before:3"],
                "{label}"
            );
            holder.success(&mut engine, "SELECT pg_advisory_unlock(611)", true);
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
                    "SELECT stage || ':' || n FROM wait_log ORDER BY n; \
                     SELECT v FROM wait_rows; \
                     SELECT funcname || ':' || calls FROM pg_stat_user_functions \
                     ORDER BY funcname",
                    true
                )),
                [
                    "outer-before:1",
                    "inner-before:3",
                    "inner-after:4",
                    "outer-after-11:5",
                    "100",
                    "wait_inner:1",
                    "wait_outer:1",
                ],
                "{label}"
            );
            drop((holder, writer, engine));
            std::fs::remove_dir_all(config.data_dir).unwrap();
        }
    }
}

/// A procedure body that reads cold rows after its own `COMMIT` yields the
/// worker while the object-store GET is delayed, lets another client run, and
/// resumes at that query: the committed insert stays, the later insert sees the
/// complete read, and nothing runs twice. Both transaction modes.
#[test]
fn procedure_storage_reads_yield_after_commit() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("procedure-cold-read-{mode:?}"));
        config.transaction_mode = mode;
        config.object_store_on = true;
        config.object_store_sim = true;
        config.object_store_bucket =
            format!("procedure-cold-read-{mode:?}-{}", std::process::id()).to_lowercase();
        // The simulator allocates while it stores uploads, so the procedure's
        // own commit stays local and the fixed-memory scope covers the call.
        config.wal_upload = false;
        config.object_store_get_slots = 4;
        config.object_store_hedge_after_ms = 0;
        config.block_cache_bytes = 0;
        config.disk_cache_bytes = 0;
        config.max_spill_generations_per_table = 2;
        config.memtable_bytes = 8 << 20;
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let namespace = crate::object_store::sim::open_namespace(&config.object_store_bucket, 229);
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(
            &mut engine,
            "CREATE TABLE cold_items(id integer, code text); \
             CREATE PROCEDURE cold_reader() LANGUAGE plpgsql AS $$ \
             DECLARE total bigint; BEGIN \
             INSERT INTO cold_log VALUES ('before'); COMMIT; \
             SELECT sum(length(code)) INTO total FROM cold_items; \
             INSERT INTO cold_log VALUES ('after-' || total); END$$",
            false,
        );
        let mut insert = String::from("INSERT INTO cold_items VALUES ");
        for id in 1..=160 {
            if id != 1 {
                insert.push(',');
            }
            insert.push_str(&format!("({id}, repeat('x', 1000))"));
        }
        session.success(&mut engine, &insert, false);
        assert!(engine.checkpoint().unwrap());
        drop((session, engine));
        std::fs::remove_dir_all(&config.data_dir).unwrap();
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
        let mut other =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut network = config.clone();
        network.object_store_sim = false;
        network.object_store_endpoint = listener.local_addr().unwrap().to_string();
        network.object_store_access_key = "test-access".to_owned();
        network.object_store_secret_key = "test-secret".to_owned();
        // The log is created after recovery, so only `cold_items` is cold.
        writer.success(&mut engine, "CREATE TABLE cold_log(stage text)", true);
        replace_uncached_blocks(&mut engine, &network);
        let prefix = format!("/{}/", config.object_store_bucket);
        let owner =
            ExecutionIdentity::new(1, engine.execution_identity().database_oid).read_owner();
        let sql = "CALL cold_reader()";
        let mut waits = 0;
        let mut completed_statements = 0;
        let mut deadline = crate::test_wait::deadline();
        writer.send.clear();
        writer.arena.reset();
        loop {
            advance(&mut engine);
            let previous = engine.replace_read_owner(Some(owner));
            let result = crate::mem::guard::forbid_alloc(|| {
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
            let _ = engine.replace_read_owner(previous);
            let ExecutionStatus::Blocked {
                completed_statements: completed,
                output_mark,
                wait,
            } = result
            else {
                break;
            };
            assert_eq!(wait, StatementWait::ObjectIo, "{mode:?}");
            output_mark.truncate_unsent(&mut writer.send);
            writer.arena.recycle_front();
            completed_statements = completed;
            if waits == 0 {
                // Another client runs while the procedure waits; the first
                // transaction is already committed.
                assert_eq!(
                    data_rows(&other.success(&mut engine, "SELECT stage FROM cold_log", true)),
                    ["before"],
                    "{mode:?}"
                );
            }
            waits += 1;
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let request = read_request(&mut engine, &mut stream);
                    let key = request
                        .lines()
                        .next()
                        .unwrap()
                        .split_whitespace()
                        .nth(1)
                        .unwrap()
                        .strip_prefix(&prefix)
                        .unwrap()
                        .to_owned();
                    let body = namespace.borrow().object_bytes(&key).unwrap().to_vec();
                    reply(&mut stream, &request, &body);
                    deadline = crate::test_wait::deadline();
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "procedure read did not advance");
                    std::thread::yield_now();
                }
                Err(error) => panic!("procedure read accept: {error}"),
            }
        }
        assert!(waits > 0, "{mode:?}: the cold read must wait");
        assert!(
            error_sqlstates(writer.send.readable()).is_empty(),
            "{mode:?}: {}",
            String::from_utf8_lossy(writer.send.readable())
        );
        assert_eq!(command_tags(writer.send.readable()), ["CALL"]);
        assert_eq!(
            // Without ORDER BY: a sort would spill to the unserved store.
            data_rows(&writer.success(
                &mut engine,
                "SELECT count(*) FILTER (WHERE stage = 'before'), \
                        max(stage) FILTER (WHERE stage <> 'before'), count(*) FROM cold_log",
                true
            )),
            ["1|after-160000|2"],
            "{mode:?}"
        );
        drop((other, writer, engine));
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

/// A lock timeout or statement timeout while a procedure waits after its own
/// `COMMIT` ends only the current transaction, as in PostgreSQL 18.6: the
/// committed rows stay, the waiting transaction's rows are rolled back, the
/// saved body is released and the session runs the procedure again.
#[test]
fn procedure_wait_failures_release_saved_frames() {
    for (label, timeout, code) in [("lock", false, "55P03"), ("statement", true, "57014")] {
        let config = test_config(&format!("procedure-wait-failure-{label}"));
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut writer =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        let mut holder =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(
            &mut engine,
            "CREATE TABLE failed_log(stage text); \
             CREATE PROCEDURE failed_wait() LANGUAGE plpgsql AS $$ BEGIN \
             INSERT INTO failed_log VALUES ('committed'); COMMIT; \
             INSERT INTO failed_log VALUES ('pending'); \
             PERFORM pg_advisory_xact_lock(711); \
             INSERT INTO failed_log VALUES ('after'); END$$",
            true,
        );
        if timeout {
            writer.success(&mut engine, "SET statement_timeout = '50ms'", true);
        }
        holder.success(&mut engine, "SELECT pg_advisory_lock(711)", true);
        let sql = "CALL failed_wait()";
        for attempt in 0..2 {
            let status = writer.execute_status(&mut engine, sql, true);
            let ExecutionStatus::Blocked {
                completed_statements,
                output_mark,
                ..
            } = status
            else {
                panic!("{label}: procedure did not wait: {status:?}");
            };
            assert!(writer.transaction.pending_procedure.is_some(), "{label}");
            if timeout {
                std::thread::sleep(Duration::from_millis(80));
            }
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
                    !timeout,
                )
            })
            .unwrap();
            assert_eq!(status, ExecutionStatus::Complete, "{label}");
            assert_eq!(
                error_sqlstates(writer.send.readable()),
                [code],
                "{label}: {}",
                String::from_utf8_lossy(writer.send.readable())
            );
            assert!(writer.transaction.pending_procedure.is_none(), "{label}");
            assert_eq!(
                data_rows(&holder.success(
                    &mut engine,
                    "SELECT stage || ':' || count(*) FROM failed_log GROUP BY stage",
                    true
                )),
                [format!("committed:{}", attempt + 1)],
                "{label}"
            );
        }
        holder.success(&mut engine, "SELECT pg_advisory_unlock(711)", true);
        writer.success(&mut engine, "RESET statement_timeout", true);
        writer.success(&mut engine, "CALL failed_wait()", true);
        assert_eq!(
            data_rows(&holder.success(
                &mut engine,
                "SELECT stage || ':' || count(*) FROM failed_log GROUP BY stage ORDER BY 1",
                true
            )),
            ["after:1", "committed:3", "pending:1"],
            "{label}"
        );
        drop((holder, writer, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

/// A procedure statement with several expressions (here `FOR` bounds) keeps
/// its finished expressions and the waiting call, before and after the
/// procedure's own `COMMIT`: each bound's insert and sequence value happen once.
#[test]
fn procedure_statements_with_several_expressions_resume() {
    for (label, commit) in [("before", ""), ("after", "COMMIT;")] {
        let config = test_config(&format!("procedure-expressions-{label}"));
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut writer =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        let mut holder =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(
            &mut engine,
            &format!(
                "CREATE SEQUENCE restart_numbers; CREATE TABLE restart_log(n bigint); \
                 CREATE FUNCTION restart_first() RETURNS bigint LANGUAGE plpgsql AS $$ \
                 DECLARE n bigint := nextval('restart_numbers'); BEGIN \
                 INSERT INTO restart_log VALUES (n); RETURN n; END$$; \
                 CREATE FUNCTION restart_child() RETURNS bigint LANGUAGE plpgsql AS $$ \
                 DECLARE n bigint := nextval('restart_numbers'); BEGIN \
                 INSERT INTO restart_log VALUES (n); PERFORM pg_advisory_xact_lock(811); \
                 RETURN n; END$$; \
                 CREATE PROCEDURE restart_proc() LANGUAGE plpgsql AS $$ BEGIN \
                 INSERT INTO restart_log VALUES (0); {commit} \
                 FOR i IN restart_first()..restart_child() LOOP \
                 INSERT INTO restart_log VALUES (-i); END LOOP; END$$"
            ),
            true,
        );
        holder.success(
            &mut engine,
            "BEGIN; SELECT pg_advisory_xact_lock(811)",
            true,
        );
        let sql = "CALL restart_proc()";
        let status = writer.execute_status(&mut engine, sql, true);
        assert!(writer.transaction.pending_procedure.is_some(), "{label}");
        holder.success(&mut engine, "COMMIT", true);
        let status = resume_procedure_wait(&mut engine, &mut writer, sql, status);
        assert_eq!(status, ExecutionStatus::Complete, "{label}");
        assert!(
            error_sqlstates(writer.send.readable()).is_empty(),
            "{label}: {}",
            String::from_utf8_lossy(writer.send.readable())
        );
        assert_eq!(
            data_rows(&writer.success(
                &mut engine,
                "SELECT n FROM restart_log ORDER BY n; SELECT last_value FROM restart_numbers",
                true
            )),
            ["-2", "-1", "0", "1", "2", "2"],
            "{label}"
        );
        drop((holder, writer, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

/// Both protocols resume a procedure that waits after its own `COMMIT` at the
/// waiting statement, and the resumed statements see the committed rows.
#[test]
fn both_protocols_resume_procedures_after_commit() {
    for extended in [false, true] {
        let label = if extended { "extended" } else { "simple" };
        let config = test_config(&format!("procedure-protocol-resume-{label}"));
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut writer =
            ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
        let mut holder =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(
            &mut engine,
            "CREATE SEQUENCE protocol_numbers; CREATE TABLE protocol_log(stage text, n bigint); \
             CREATE PROCEDURE protocol_wait(INOUT total bigint) LANGUAGE plpgsql AS $$ BEGIN \
             INSERT INTO protocol_log VALUES ('before', nextval('protocol_numbers')); COMMIT; \
             PERFORM pg_advisory_xact_lock(911); \
             INSERT INTO protocol_log VALUES ('after', nextval('protocol_numbers')); \
             SELECT sum(n) INTO total FROM protocol_log; END$$",
            true,
        );
        holder.success(&mut engine, "SELECT pg_advisory_lock(911)", true);
        let sql = "CALL protocol_wait(NULL)";
        let execute = |engine: &mut Engine,
                       writer: &mut ConfiguredTransactionSession,
                       skip: usize|
         -> ExecutionStatus {
            crate::mem::guard::forbid_alloc(|| {
                let mut responder = Responder::new(&mut writer.send);
                if !extended {
                    return engine
                        .execute_simple_from(
                            sql,
                            skip,
                            &writer.arena,
                            &mut writer.transaction,
                            &mut writer.pool,
                            &mut writer.cursors,
                            &writer.guc,
                            &mut responder,
                            writer.connection_id,
                            false,
                        )
                        .unwrap();
                }
                match engine
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
                        false,
                    )
                    .unwrap()
                {
                    ExtendedExecutionStatus::Complete(_) => ExecutionStatus::Complete,
                    ExtendedExecutionStatus::Blocked { wait } => ExecutionStatus::Blocked {
                        completed_statements: 0,
                        output_mark: responder.mark(),
                        wait,
                    },
                }
            })
        };
        writer.send.clear();
        writer.arena.reset();
        let status = execute(&mut engine, &mut writer, 0);
        let ExecutionStatus::Blocked {
            completed_statements,
            wait: StatementWait::Lock,
            ..
        } = status
        else {
            panic!(
                "{label}: {status:?}: {}",
                String::from_utf8_lossy(writer.send.readable())
            );
        };
        assert!(writer.transaction.pending_procedure.is_some(), "{label}");
        holder.success(&mut engine, "SELECT pg_advisory_unlock(911)", true);
        writer.arena.recycle_front();
        let status = execute(&mut engine, &mut writer, completed_statements);
        assert_eq!(
            status,
            ExecutionStatus::Complete,
            "{label}: {}",
            String::from_utf8_lossy(writer.send.readable())
        );
        assert!(
            error_sqlstates(writer.send.readable()).is_empty(),
            "{label}: {}",
            String::from_utf8_lossy(writer.send.readable())
        );
        assert_eq!(data_rows(writer.send.readable()), ["3"], "{label}");
        assert_eq!(
            data_rows(&writer.success(
                &mut engine,
                "SELECT stage || ':' || n FROM protocol_log ORDER BY n",
                true
            )),
            ["before:1", "after:2"],
            "{label}"
        );
        drop((holder, writer, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

/// A domain `CHECK` on a PL/pgSQL function's result calls a routine that
/// waits for a lock. The finished result and the check's calls are kept, so
/// the body's insert, the check's insert and each sequence value happen once:
/// from a top-level `SELECT`, from `DO`, and from a procedure after its own
/// `COMMIT`, in both transaction modes.
#[test]
fn domain_result_checks_resume_waiting_routines() {
    for (label, sql, tag) in [
        ("select", "SELECT checked_result(3)", "SELECT 1"),
        (
            "do",
            "DO $$DECLARE v int; BEGIN v := checked_result(3); \
             INSERT INTO check_log VALUES ('do-' || v, 0); END$$",
            "DO",
        ),
        ("call", "CALL checked_proc()", "CALL"),
    ] {
        for mode in [
            crate::config::TransactionMode::Mvcc,
            crate::config::TransactionMode::Locking,
        ] {
            let mut config = test_config(&format!("domain-check-wait-{label}-{mode:?}"));
            config.transaction_mode = mode;
            let mut budget = test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut writer =
                ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 4 << 20);
            let mut holder =
                ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
            writer.success(
                &mut engine,
                "CREATE SEQUENCE check_numbers; CREATE TABLE check_log(stage text, n bigint); \
                 CREATE FUNCTION check_small(v integer) RETURNS boolean LANGUAGE plpgsql AS $$ \
                 BEGIN INSERT INTO check_log VALUES ('check-' || v, nextval('check_numbers')); \
                 PERFORM pg_advisory_xact_lock(1011); RETURN v < 10; END$$; \
                 CREATE DOMAIN checked_small AS integer CHECK (check_small(VALUE)); \
                 CREATE FUNCTION checked_result(v integer) RETURNS checked_small \
                 LANGUAGE plpgsql AS $$ BEGIN \
                 INSERT INTO check_log VALUES ('body-' || v, nextval('check_numbers')); \
                 RETURN v; END$$; \
                 CREATE PROCEDURE checked_proc() LANGUAGE plpgsql AS $$ DECLARE v int; BEGIN \
                 INSERT INTO check_log VALUES ('proc', nextval('check_numbers')); COMMIT; \
                 v := checked_result(3); INSERT INTO check_log VALUES ('proc-' || v, 0); END$$",
                true,
            );
            holder.success(
                &mut engine,
                "BEGIN; SELECT pg_advisory_xact_lock(1011)",
                true,
            );
            let status = writer.execute_status(&mut engine, sql, true);
            assert_eq!(
                blocked_wait(&status, &writer),
                StatementWait::Lock,
                "{label}"
            );
            assert_eq!(
                data_rows(&holder.success(&mut engine, "SELECT 40 + 2", true)),
                ["42"]
            );
            holder.success(&mut engine, "COMMIT", true);
            let status = resume_procedure_wait(&mut engine, &mut writer, sql, status);
            assert_eq!(status, ExecutionStatus::Complete, "{label}");
            assert!(
                error_sqlstates(writer.send.readable()).is_empty(),
                "{label}: {}",
                String::from_utf8_lossy(writer.send.readable())
            );
            assert_eq!(command_tags(writer.send.readable()), [tag], "{label}");
            if label == "select" {
                assert_eq!(data_rows(writer.send.readable()), ["3"]);
            }
            let expected: &[&str] = match label {
                "select" => &["body-3:1", "check-3:2"],
                "do" => &["do-3:0", "body-3:1", "check-3:2"],
                _ => &["proc-3:0", "proc:1", "body-3:2", "check-3:3"],
            };
            assert_eq!(
                data_rows(&writer.success(
                    &mut engine,
                    "SELECT stage || ':' || n FROM check_log ORDER BY n, stage",
                    true
                )),
                expected,
                "{label}"
            );
            drop((holder, writer, engine));
            std::fs::remove_dir_all(config.data_dir).unwrap();
        }
    }
}
