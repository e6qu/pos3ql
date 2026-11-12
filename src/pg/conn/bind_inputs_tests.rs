use super::*;

fn frontend(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(payload.len() + 5);
    message.push(kind);
    message.extend_from_slice(&((payload.len() + 4) as i32).to_be_bytes());
    message.extend_from_slice(payload);
    message
}

fn has_message(mut bytes: &[u8], kind: u8) -> bool {
    while bytes.len() >= 5 {
        let total = 1 + i32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize;
        assert!(total >= 5 && total <= bytes.len());
        if bytes[0] == kind {
            return true;
        }
        bytes = &bytes[total..];
    }
    assert!(bytes.is_empty());
    false
}

fn first_row(mut bytes: &[u8]) -> String {
    while bytes.len() >= 5 {
        let total = 1 + i32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize;
        assert!(total >= 5 && total <= bytes.len());
        if bytes[0] == wire::MSG_DATA_ROW {
            let count = u16::from_be_bytes(bytes[5..7].try_into().unwrap());
            let mut row = String::new();
            let mut at = 7;
            for column in 0..count {
                if column != 0 {
                    row.push('|');
                }
                let length = i32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
                at += 4;
                if length < 0 {
                    row.push_str("NULL");
                } else {
                    row.push_str(core::str::from_utf8(&bytes[at..at + length as usize]).unwrap());
                    at += length as usize;
                }
            }
            return row;
        }
        bytes = &bytes[total..];
    }
    panic!("missing DataRow")
}

fn message(conn: &mut Conn, engine: &mut Engine, kind: u8, payload: &[u8], error: bool) -> Vec<u8> {
    conn.send.clear();
    assert!(conn.recv.append(&frontend(kind, payload)));
    let step = crate::mem::guard::forbid_alloc(|| conn.process_message(engine));
    assert!(
        matches!(step, Step::Continue),
        "frontend {} {:?}, step {step:?}: {}, fatal {:?}",
        char::from(kind),
        payload,
        String::from_utf8_lossy(conn.send.readable()),
        engine.fatal_commit_failure()
    );
    let response = conn.send.readable().to_vec();
    assert_eq!(
        has_message(&response, wire::MSG_ERROR_RESPONSE),
        error,
        "frontend {} {:?}: {}, transaction {:?}, fatal {:?}",
        char::from(kind),
        payload,
        String::from_utf8_lossy(&response),
        conn.txn.mode,
        engine.fatal_commit_failure()
    );
    response
}

fn query(conn: &mut Conn, engine: &mut Engine, sql: &str) -> Vec<u8> {
    let mut payload = sql.as_bytes().to_vec();
    payload.push(0);
    message(conn, engine, wire::FMSG_QUERY, &payload, false)
}

fn parse(conn: &mut Conn, engine: &mut Engine, sql: &str, oids: &[i32]) {
    let mut payload = b"bound\0".to_vec();
    payload.extend_from_slice(sql.as_bytes());
    payload.push(0);
    payload.extend_from_slice(&(oids.len() as u16).to_be_bytes());
    for oid in oids {
        payload.extend_from_slice(&oid.to_be_bytes());
    }
    assert!(has_message(
        &message(conn, engine, wire::FMSG_PARSE, &payload, false),
        wire::MSG_PARSE_COMPLETE
    ));
}

fn bind_payload(portal: &str, parameters: &[Option<&[u8]>]) -> Vec<u8> {
    let mut payload = portal.as_bytes().to_vec();
    payload.extend_from_slice(b"\0bound\0");
    payload.extend_from_slice(&0u16.to_be_bytes());
    payload.extend_from_slice(&(parameters.len() as u16).to_be_bytes());
    for parameter in parameters {
        match parameter {
            Some(parameter) => {
                payload.extend_from_slice(&(parameter.len() as i32).to_be_bytes());
                payload.extend_from_slice(parameter);
            }
            None => payload.extend_from_slice(&(-1i32).to_be_bytes()),
        }
    }
    payload.extend_from_slice(&0u16.to_be_bytes());
    payload
}

fn config(name: &str, mode: crate::config::TransactionMode) -> Config {
    let mut config = Config::default_dev();
    config.data_dir = std::env::temp_dir()
        .join(format!("pos3ql-{name}-{}-{mode:?}", std::process::id()))
        .to_string_lossy()
        .into_owned();
    config.remove_test_data_dir_on_drop();
    config.transaction_mode = mode;
    config.max_connections = 3;
    config.max_tables = 8;
    config.row_overlay_slots = 128;
    config.max_prepared = 2;
    config.max_portals = 2;
    config.prepared_bytes = 32 << 10;
    config
}

fn parse_command(conn: &mut Conn, engine: &mut Engine, sql: &str) {
    let mut payload = vec![0];
    payload.extend_from_slice(sql.as_bytes());
    payload.extend_from_slice(&[0, 0, 0]);
    assert!(has_message(
        &message(conn, engine, wire::FMSG_PARSE, &payload, false),
        wire::MSG_PARSE_COMPLETE
    ));
}

fn bind_command(conn: &mut Conn, engine: &mut Engine, sql: &str) {
    parse_command(conn, engine, sql);
    assert!(has_message(
        &message(conn, engine, wire::FMSG_BIND, &[0; 8], false),
        wire::MSG_BIND_COMPLETE
    ));
}

#[test]
fn prepared_resolution_distinguishes_bind_from_an_executed_pipeline() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = config("bind-prepared-resolution", mode);
        config.max_prepared_transactions = 1;
        let mut budget = crate::sql::test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut conn = Conn::new(&config, &mut budget).unwrap();
        conn.id = 1;
        conn.enter_ready_for_tests(&engine);
        query(
            &mut conn,
            &mut engine,
            "CREATE TABLE bind_prepared(id integer PRIMARY KEY, value text); \
             INSERT INTO bind_prepared VALUES(1,'before')",
        );
        for commit in [false, true] {
            for context in ["first", "pipeline", "explicit"] {
                query(
                    &mut conn,
                    &mut engine,
                    "UPDATE bind_prepared SET value='before' WHERE id=1",
                );
                query(
                    &mut conn,
                    &mut engine,
                    "BEGIN; \
                     UPDATE bind_prepared SET value='after' WHERE id=1; \
                     PREPARE TRANSACTION 'bind-held'",
                );
                assert_eq!(
                    first_row(&query(
                        &mut conn,
                        &mut engine,
                        "SELECT gid FROM pg_prepared_xacts"
                    )),
                    "bind-held"
                );
                match mode {
                    crate::config::TransactionMode::Mvcc => assert_eq!(
                        first_row(&query(
                            &mut conn,
                            &mut engine,
                            "SELECT value FROM bind_prepared WHERE id=1"
                        )),
                        "before"
                    ),
                    crate::config::TransactionMode::Locking => {
                        conn.send.clear();
                        assert!(conn.recv.append(&frontend(
                            wire::FMSG_QUERY,
                            b"SELECT value FROM bind_prepared WHERE id=1\0"
                        )));
                        let step =
                            crate::mem::guard::forbid_alloc(|| conn.process_message(&mut engine));
                        assert!(
                            matches!(step, Step::Parked),
                            "{step:?}: {}",
                            String::from_utf8_lossy(conn.send.readable())
                        );
                        assert!(crate::mem::guard::forbid_alloc(
                            || conn.cancel_parked(&mut engine)
                        ));
                        assert!(
                            conn.send
                                .readable()
                                .windows(5)
                                .any(|bytes| bytes == b"57014")
                        );
                        assert_eq!(conn.txn.mode, crate::sql::txn::TxnMode::Idle);
                    }
                }
                match context {
                    "pipeline" => {
                        bind_command(&mut conn, &mut engine, "SELECT 1");
                        assert_eq!(
                            first_row(&message(
                                &mut conn,
                                &mut engine,
                                wire::FMSG_EXECUTE,
                                &[0; 5],
                                false
                            )),
                            "1"
                        );
                        assert_eq!(conn.txn.mode, crate::sql::txn::TxnMode::ImplicitBlock);
                    }
                    "explicit" => {
                        query(&mut conn, &mut engine, "BEGIN");
                    }
                    "first" => {}
                    _ => unreachable!(),
                }
                let sql = if commit {
                    "COMMIT PREPARED 'bind-held'"
                } else {
                    "ROLLBACK PREPARED 'bind-held'"
                };
                bind_command(&mut conn, &mut engine, sql);
                if context == "first" {
                    assert_eq!(conn.txn.mode, crate::sql::txn::TxnMode::Implicit);
                    assert!(!conn.txn.snapshot_taken());
                }
                let blocked = context != "first";
                let response =
                    message(&mut conn, &mut engine, wire::FMSG_EXECUTE, &[0; 5], blocked);
                if blocked {
                    assert!(response.windows(5).any(|bytes| bytes == b"25001"));
                    assert!(matches!(conn.phase, Phase::SkipToSync));
                    assert!(!conn.guc.seq_session().retains_statement_effects());
                } else {
                    assert!(has_message(&response, wire::MSG_COMMAND_COMPLETE));
                    assert_eq!(conn.txn.mode, crate::sql::txn::TxnMode::Implicit);
                }
                message(&mut conn, &mut engine, wire::FMSG_SYNC, &[], false);
                assert!(matches!(conn.phase, Phase::Ready));
                if context == "explicit" {
                    assert_eq!(conn.txn.status_byte(), b'E');
                    bind_command(&mut conn, &mut engine, "ROLLBACK");
                    message(&mut conn, &mut engine, wire::FMSG_EXECUTE, &[0; 5], false);
                    message(&mut conn, &mut engine, wire::FMSG_SYNC, &[], false);
                }
                assert_eq!(conn.txn.mode, crate::sql::txn::TxnMode::Idle);
                if blocked {
                    assert_eq!(
                        first_row(&query(
                            &mut conn,
                            &mut engine,
                            "SELECT gid FROM pg_prepared_xacts"
                        )),
                        "bind-held"
                    );
                    bind_command(&mut conn, &mut engine, sql);
                    assert!(!conn.txn.snapshot_taken());
                    message(&mut conn, &mut engine, wire::FMSG_EXECUTE, &[0; 5], false);
                    message(&mut conn, &mut engine, wire::FMSG_SYNC, &[], false);
                }
                assert_eq!(conn.txn.mode, crate::sql::txn::TxnMode::Idle);
                assert_eq!(
                    first_row(&query(
                        &mut conn,
                        &mut engine,
                        "SELECT count(*) FROM pg_prepared_xacts"
                    )),
                    "0"
                );
                assert_eq!(
                    first_row(&query(
                        &mut conn,
                        &mut engine,
                        "SELECT value FROM bind_prepared WHERE id=1"
                    )),
                    if commit { "after" } else { "before" }
                );
            }
        }
    }
}

#[test]
fn immediate_utilities_publish_before_sync_with_postgresql_completion_tags() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let config = config("immediate-utility-publication", mode);
        let mut budget = crate::sql::test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut conn = Conn::new(&config, &mut budget).unwrap();
        let mut observer = Conn::new(&config, &mut budget).unwrap();
        conn.id = 1;
        observer.id = 2;
        conn.enter_ready_for_tests(&engine);
        observer.enter_ready_for_tests(&engine);
        for (sql, tag, observation, expected) in [
            (
                "CREATE DATABASE utility_database",
                "CREATE DATABASE",
                "SELECT count(*) FROM pg_database WHERE datname='utility_database'",
                "1",
            ),
            (
                "DROP DATABASE utility_database",
                "DROP DATABASE",
                "SELECT count(*) FROM pg_database WHERE datname='utility_database'",
                "0",
            ),
        ] {
            bind_command(&mut conn, &mut engine, sql);
            let response = message(&mut conn, &mut engine, wire::FMSG_EXECUTE, &[0; 5], false);
            assert_eq!(command_completion_in(&response), Some(tag), "{sql}");
            assert_eq!(conn.txn.mode, crate::sql::txn::TxnMode::Idle, "{sql}");
            assert_eq!(
                first_row(&query(&mut observer, &mut engine, observation)),
                expected,
                "{sql}"
            );
            message(&mut conn, &mut engine, wire::FMSG_SYNC, &[], false);
        }
    }
}

#[test]
#[cfg(not(miri))]
fn immediate_utility_resumes_durable_commit_without_repeating_ddl() {
    let mut config = config(
        "immediate-utility-wait",
        crate::config::TransactionMode::Mvcc,
    );
    config.object_store_on = true;
    config.object_store_bucket = format!("immediate-utility-wait-{}", std::process::id());
    config.wal_upload = true;
    config.wal_upload_sync = true;
    config.wal_upload_buffer_bytes = 1 << 20;
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    let server = crate::sql::HttpObjectServer::start(&mut config);
    let mut budget = crate::sql::test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut conn = Conn::new(&config, &mut budget).unwrap();
    conn.id = 1;
    conn.enter_ready_for_tests(&engine);
    query(
        &mut conn,
        &mut engine,
        "CREATE TABLE utility_wait_rows(id integer); INSERT INTO utility_wait_rows VALUES(1)",
    );
    engine.commit_wal().unwrap();
    engine.enable_async_durability();
    bind_command(
        &mut conn,
        &mut engine,
        "CREATE DATABASE utility_wait_database",
    );
    conn.send.clear();
    assert!(conn.recv.append(&frontend(wire::FMSG_EXECUTE, &[0; 5])));
    let mut transaction = None;
    for _ in 0..3 {
        assert!(
            matches!(
                crate::mem::guard::forbid_alloc(|| conn.process_message(&mut engine)),
                Step::Parked
            ),
            "{}",
            String::from_utf8_lossy(conn.send.readable())
        );
        let staged = conn
            .txn
            .staged_commit()
            .expect("completed utility retains its commit");
        assert_eq!(
            staged.target,
            crate::sql::txn::CommitTarget::Utility(crate::sql::UtilityCommand::CreateDatabase)
        );
        assert_eq!(*transaction.get_or_insert(conn.txn.txid), conn.txn.txid);
        assert!(!has_message(
            conn.send.readable(),
            wire::MSG_COMMAND_COMPLETE
        ));
        assert!(!has_message(conn.send.readable(), wire::MSG_ERROR_RESPONSE));
    }
    let wrong_target = crate::mem::guard::forbid_alloc(|| {
        engine.commit_txn_with_triggers(
            &mut conn.txn,
            &conn.guc,
            crate::sql::txn::CommitTarget::Ordinary,
            None,
            &conn.arena,
            &mut Responder::new(&mut conn.send),
        )
    })
    .unwrap_err();
    assert_eq!(wrong_target.sqlstate, sqlstate::INTERNAL_ERROR);
    assert!(conn.txn.staged_commit().is_some());
    engine.commit_wal().unwrap();
    assert!(
        matches!(
            crate::mem::guard::forbid_alloc(|| conn.process_message(&mut engine)),
            Step::Continue
        ),
        "{}",
        String::from_utf8_lossy(conn.send.readable())
    );
    assert_eq!(
        command_completion_in(conn.send.readable()),
        Some("CREATE DATABASE")
    );
    assert_eq!(conn.txn.mode, crate::sql::txn::TxnMode::Idle);
    message(&mut conn, &mut engine, wire::FMSG_SYNC, &[], false);
    assert_eq!(
        first_row(&query(
            &mut conn,
            &mut engine,
            "SELECT count(*) FROM pg_database WHERE datname='utility_wait_database'"
        )),
        "1"
    );
    drop(conn);
    drop(engine);
    std::fs::remove_dir_all(&config.data_dir).unwrap();
    let mut recovered_budget = crate::sql::test_engine_budget(&config, 1 << 29);
    let mut recovered = Engine::new(&config, &mut recovered_budget).unwrap();
    let mut recovered_conn = Conn::new(&config, &mut recovered_budget).unwrap();
    recovered_conn.id = 1;
    recovered_conn.enter_ready_for_tests(&recovered);
    assert_eq!(
        first_row(&query(
            &mut recovered_conn,
            &mut recovered,
            "SELECT count(*) FROM pg_database WHERE datname='utility_wait_database'"
        )),
        "1"
    );
    drop(recovered_conn);
    drop(recovered);
    drop(server);
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
}

#[test]
fn duplicate_named_bind_aborts_the_block_and_rollback_releases_the_portal() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let config = config("duplicate-bind-recovery", mode);
        let mut budget = crate::sql::test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut conn = Conn::new(&config, &mut budget).unwrap();
        conn.id = 1;
        conn.enter_ready_for_tests(&engine);
        query(&mut conn, &mut engine, "BEGIN");
        parse(&mut conn, &mut engine, "SELECT 7", &[]);
        message(
            &mut conn,
            &mut engine,
            wire::FMSG_BIND,
            &bind_payload("duplicate_portal", &[]),
            false,
        );
        let duplicate = message(
            &mut conn,
            &mut engine,
            wire::FMSG_BIND,
            &bind_payload("duplicate_portal", &[]),
            true,
        );
        assert!(duplicate.windows(5).any(|bytes| bytes == b"42P03"));
        assert!(
            duplicate
                .windows(b"cursor \"duplicate_portal\" already exists".len())
                .any(|bytes| bytes == b"cursor \"duplicate_portal\" already exists")
        );
        message(&mut conn, &mut engine, wire::FMSG_SYNC, &[], false);
        assert_eq!(conn.txn.status_byte(), b'E');
        let execute = b"duplicate_portal\0\0\0\0\0";
        let failed = message(&mut conn, &mut engine, wire::FMSG_EXECUTE, execute, true);
        assert!(failed.windows(5).any(|bytes| bytes == b"25P02"));
        message(&mut conn, &mut engine, wire::FMSG_SYNC, &[], false);
        query(&mut conn, &mut engine, "ROLLBACK");
        assert_eq!(conn.txn.status_byte(), b'I');
        assert!(conn.portals.iter().all(|portal| !portal.active));
        let gone = message(&mut conn, &mut engine, wire::FMSG_EXECUTE, execute, true);
        assert!(gone.windows(5).any(|bytes| bytes == b"34000"));
        message(&mut conn, &mut engine, wire::FMSG_SYNC, &[], false);
        assert_eq!(first_row(&query(&mut conn, &mut engine, "SELECT 9")), "9");
    }
}

#[test]
fn extended_parse_errors_abort_implicit_work_and_preserve_explicit_failure() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let config = config("parse-error-transaction", mode);
        let mut budget = crate::sql::test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut conn = Conn::new(&config, &mut budget).unwrap();
        conn.id = 1;
        conn.enter_ready_for_tests(&engine);
        query(
            &mut conn,
            &mut engine,
            "CREATE TABLE parse_error_values(value integer)",
        );
        for explicit in [false, true] {
            if explicit {
                query(&mut conn, &mut engine, "BEGIN");
            }
            bind_command(
                &mut conn,
                &mut engine,
                "INSERT INTO parse_error_values VALUES(1)",
            );
            message(&mut conn, &mut engine, wire::FMSG_EXECUTE, &[0; 5], false);
            let error = message(
                &mut conn,
                &mut engine,
                wire::FMSG_PARSE,
                b"\0SELECT (\0\0\0",
                true,
            );
            assert!(error.windows(5).any(|bytes| bytes == b"42601"));
            assert!(!conn.guc.seq_session().retains_statement_effects());
            assert_eq!(conn.txn.status_byte(), if explicit { b'E' } else { b'I' });
            message(&mut conn, &mut engine, wire::FMSG_SYNC, &[], false);
            if explicit {
                bind_command(&mut conn, &mut engine, "ROLLBACK");
                message(&mut conn, &mut engine, wire::FMSG_EXECUTE, &[0; 5], false);
                message(&mut conn, &mut engine, wire::FMSG_SYNC, &[], false);
            }
            assert_eq!(conn.txn.mode, crate::sql::txn::TxnMode::Idle);
            assert_eq!(
                first_row(&query(
                    &mut conn,
                    &mut engine,
                    "SELECT count(*) FROM parse_error_values"
                )),
                "0"
            );
        }
    }
}

#[test]
fn binding_transaction_settings_does_not_take_a_data_snapshot() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let config = config("bind-transaction-snapshot", mode);
        let mut budget = crate::sql::test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut conn = Conn::new(&config, &mut budget).unwrap();
        conn.id = 1;
        conn.enter_ready_for_tests(&engine);
        query(&mut conn, &mut engine, "BEGIN");
        bind_command(
            &mut conn,
            &mut engine,
            "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED",
        );
        assert!(!conn.txn.snapshot_taken());
        message(&mut conn, &mut engine, wire::FMSG_EXECUTE, &[0; 5], false);
        message(&mut conn, &mut engine, wire::FMSG_SYNC, &[], false);
        assert!(!conn.txn.snapshot_taken());
        message(&mut conn, &mut engine, wire::FMSG_CLOSE, b"P\0", false);
        parse_command(&mut conn, &mut engine, "SELECT 1");
        assert!(conn.txn.snapshot_taken());
        bind_command(
            &mut conn,
            &mut engine,
            "SET TRANSACTION ISOLATION LEVEL READ COMMITTED",
        );
        let error = message(&mut conn, &mut engine, wire::FMSG_EXECUTE, &[0; 5], true);
        assert!(error.windows(5).any(|bytes| bytes == b"25001"));
        message(&mut conn, &mut engine, wire::FMSG_SYNC, &[], false);
        assert_eq!(conn.txn.status_byte(), b'E');
        query(&mut conn, &mut engine, "ROLLBACK");

        parse(&mut conn, &mut engine, "SELECT 1", &[]);
        message(&mut conn, &mut engine, wire::FMSG_SYNC, &[], false);
        assert_eq!(conn.txn.mode, crate::sql::txn::TxnMode::Idle);
        query(&mut conn, &mut engine, "BEGIN");
        assert!(!conn.txn.snapshot_taken());
        message(
            &mut conn,
            &mut engine,
            wire::FMSG_BIND,
            &bind_payload("", &[]),
            false,
        );
        assert!(conn.txn.snapshot_taken());
        message(&mut conn, &mut engine, wire::FMSG_EXECUTE, &[0; 5], false);
        message(&mut conn, &mut engine, wire::FMSG_SYNC, &[], false);
        message(&mut conn, &mut engine, wire::FMSG_CLOSE, b"P\0", false);
        message(&mut conn, &mut engine, wire::FMSG_CLOSE, b"Sbound\0", false);
        assert!(conn.txn.snapshot_taken());
        bind_command(
            &mut conn,
            &mut engine,
            "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED",
        );
        let error = message(&mut conn, &mut engine, wire::FMSG_EXECUTE, &[0; 5], true);
        assert!(error.windows(5).any(|bytes| bytes == b"25001"));
        message(&mut conn, &mut engine, wire::FMSG_SYNC, &[], false);
        query(&mut conn, &mut engine, "ROLLBACK");

        if mode == crate::config::TransactionMode::Mvcc {
            let position = engine.replication_identity().1;
            let snapshot = crate::mem::guard::forbid_alloc(|| {
                engine.export_replication_snapshot(2, position).unwrap()
            });
            query(
                &mut conn,
                &mut engine,
                "BEGIN ISOLATION LEVEL REPEATABLE READ",
            );
            bind_command(
                &mut conn,
                &mut engine,
                &format!("SET TRANSACTION SNAPSHOT '{}'", snapshot.as_str()),
            );
            assert!(!conn.txn.snapshot_taken());
            message(&mut conn, &mut engine, wire::FMSG_EXECUTE, &[0; 5], false);
            message(&mut conn, &mut engine, wire::FMSG_SYNC, &[], false);
            assert!(conn.txn.snapshot_taken());
            assert_eq!(conn.txn.snapshot_lsn(), Some(position));
            query(&mut conn, &mut engine, "ROLLBACK");
            engine.invalidate_replication_snapshot(2);
        }
    }
}

#[test]
fn checked_function_planning_happens_at_bind_after_parameter_validation() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let config = config("bind-planner-inputs", mode);
        let mut budget = crate::sql::test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut conn = Conn::new(&config, &mut budget).unwrap();
        let mut observer = Conn::new(&config, &mut budget).unwrap();
        conn.id = 1;
        observer.id = 2;
        conn.enter_ready_for_tests(&engine);
        observer.enter_ready_for_tests(&engine);
        query(
            &mut conn,
            &mut engine,
            "CREATE SEQUENCE bind_checks; CREATE TYPE bind_choice AS ENUM('a'); \
             CREATE DOMAIN bind_checked AS bind_choice CHECK(nextval('bind_checks')>0); \
             CREATE FUNCTION bind_scalar(integer) RETURNS integer LANGUAGE sql AS \
             $$SELECT $1+cardinality('{a}'::bind_checked[])$$; \
             ALTER SEQUENCE bind_checks RESTART WITH 1",
        );
        parse(
            &mut conn,
            &mut engine,
            "SELECT bind_scalar($1)",
            &[crate::sql::types::oid::INT4],
        );
        assert_eq!(
            first_row(&query(
                &mut observer,
                &mut engine,
                "SELECT is_called FROM bind_checks"
            )),
            "f"
        );
        let invalid = message(
            &mut conn,
            &mut engine,
            wire::FMSG_BIND,
            &bind_payload("result", &[Some(b"invalid")]),
            true,
        );
        assert!(invalid.windows(5).any(|bytes| bytes == b"22P02"));
        assert!(conn.pending_bind.is_none());
        assert!(conn.portals.iter().all(|portal| !portal.active));
        assert_eq!(
            first_row(&query(
                &mut observer,
                &mut engine,
                "SELECT is_called FROM bind_checks"
            )),
            "f"
        );
        message(&mut conn, &mut engine, wire::FMSG_SYNC, &[], false);
        assert!(has_message(
            &message(
                &mut conn,
                &mut engine,
                wire::FMSG_BIND,
                &bind_payload("result", &[Some(b"9")]),
                false
            ),
            wire::MSG_BIND_COMPLETE
        ));
        assert_eq!(
            first_row(&query(
                &mut observer,
                &mut engine,
                "SELECT last_value,is_called FROM bind_checks"
            )),
            "1|t"
        );
        let slot = conn
            .portals
            .iter()
            .position(|portal| portal.active)
            .unwrap();
        let retained = conn.portals[slot].params.readable().to_vec();
        let output = message(
            &mut conn,
            &mut engine,
            wire::FMSG_EXECUTE,
            b"result\0\0\0\0\0",
            false,
        );
        assert_eq!(first_row(&output), "10");
        assert!(has_message(&output, wire::MSG_COMMAND_COMPLETE));
        assert_eq!(conn.portals[slot].params.readable(), retained);
        message(&mut conn, &mut engine, wire::FMSG_SYNC, &[], false);
        assert_eq!(
            first_row(&query(
                &mut observer,
                &mut engine,
                "SELECT last_value FROM bind_checks"
            )),
            "1"
        );
        message(
            &mut conn,
            &mut engine,
            wire::FMSG_BIND,
            &bind_payload("result", &[Some(b"20")]),
            false,
        );
        assert_eq!(
            first_row(&query(
                &mut observer,
                &mut engine,
                "SELECT last_value FROM bind_checks"
            )),
            "2"
        );
        assert_eq!(
            first_row(&message(
                &mut conn,
                &mut engine,
                wire::FMSG_EXECUTE,
                b"result\0\0\0\0\0",
                false
            )),
            "21"
        );
        message(&mut conn, &mut engine, wire::FMSG_SYNC, &[], false);
    }
}

#[test]
fn bind_planner_waits_keep_unpublished_parameters_and_release_them_on_abort() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for (cancel, timeout) in [(false, false), (true, false), (false, true)] {
            let config = config(&format!("bind-input-waits-{cancel}-{timeout}"), mode);
            let mut budget = crate::sql::test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut conn = Conn::new(&config, &mut budget).unwrap();
            let mut holder = Conn::new(&config, &mut budget).unwrap();
            let mut observer = Conn::new(&config, &mut budget).unwrap();
            conn.id = 1;
            holder.id = 2;
            observer.id = 3;
            for connection in [&mut conn, &mut holder, &mut observer] {
                connection.enter_ready_for_tests(&engine);
            }
            query(
                &mut conn,
                &mut engine,
                "CREATE SEQUENCE bind_wait_checks; CREATE TABLE bind_wait_gate(id integer PRIMARY KEY,hits integer); \
                 INSERT INTO bind_wait_gate VALUES(1,0); CREATE TYPE bind_wait_choice AS ENUM('a'); \
                 CREATE FUNCTION bind_wait_allowed(value bind_wait_choice) RETURNS boolean LANGUAGE plpgsql AS $$BEGIN \
                   PERFORM nextval('bind_wait_checks'); UPDATE bind_wait_gate SET hits=hits+1 WHERE id=1; RETURN true; END$$; \
                 CREATE DOMAIN bind_wait_checked AS bind_wait_choice CHECK(bind_wait_allowed(VALUE)); \
                 CREATE FUNCTION bind_wait_scalar(integer) RETURNS integer LANGUAGE sql AS \
                   $$SELECT $1+cardinality('{a}'::bind_wait_checked[])$$; \
                 ALTER SEQUENCE bind_wait_checks RESTART WITH 1; UPDATE bind_wait_gate SET hits=0; SET lock_timeout='10s'",
            );
            parse(
                &mut conn,
                &mut engine,
                "SELECT bind_wait_scalar($1)",
                &[crate::sql::types::oid::INT4],
            );
            query(
                &mut holder,
                &mut engine,
                "BEGIN; UPDATE bind_wait_gate SET hits=hits WHERE id=1",
            );
            conn.send.clear();
            let bind = frontend(wire::FMSG_BIND, &bind_payload("result", &[Some(b"9")]));
            assert!(conn.recv.append(&bind));
            let mut deadline = None;
            let mut retained = None;
            for attempt in 0..3 {
                assert!(matches!(
                    crate::mem::guard::forbid_alloc(|| conn.process_message(&mut engine)),
                    Step::Parked
                ));
                assert!(conn.send.is_empty());
                assert_eq!(conn.recv.readable(), bind);
                assert!(conn.portals.iter().all(|portal| !portal.active));
                let slot = conn
                    .pending_bind
                    .expect("Bind retains a provisional portal slot");
                if attempt == 0 {
                    deadline = conn.parked_deadline;
                    assert!(deadline.is_some());
                    retained = Some(conn.portals[slot].params.readable().to_vec());
                } else {
                    assert_eq!(conn.parked_deadline, deadline);
                    assert_eq!(
                        conn.portals[slot].params.readable(),
                        retained.as_ref().unwrap()
                    );
                }
            }
            assert_eq!(
                first_row(&query(
                    &mut observer,
                    &mut engine,
                    "SELECT last_value,is_called FROM bind_wait_checks"
                )),
                "1|t"
            );
            if cancel {
                assert!(conn.recv.append(&frontend(wire::FMSG_SYNC, &[])));
                assert!(crate::mem::guard::forbid_alloc(
                    || conn.cancel_parked(&mut engine)
                ));
                assert!(
                    conn.send
                        .readable()
                        .windows(5)
                        .any(|bytes| bytes == b"57014")
                );
                assert!(has_message(conn.send.readable(), wire::MSG_READY_FOR_QUERY));
            } else if timeout {
                conn.parked_deadline = Some(Instant::now());
                assert!(matches!(
                    crate::mem::guard::forbid_alloc(|| conn.process_message(&mut engine)),
                    Step::Continue
                ));
                assert!(
                    conn.send
                        .readable()
                        .windows(5)
                        .any(|bytes| bytes == b"55P03")
                );
            }
            query(&mut holder, &mut engine, "COMMIT");
            if !cancel && !timeout {
                assert!(matches!(
                    crate::mem::guard::forbid_alloc(|| conn.process_message(&mut engine)),
                    Step::Continue
                ));
                assert!(has_message(conn.send.readable(), wire::MSG_BIND_COMPLETE));
                assert!(!has_message(conn.send.readable(), wire::MSG_ERROR_RESPONSE));
                assert!(conn.pending_bind.is_none());
                let slot = conn
                    .portals
                    .iter()
                    .position(|portal| portal.active)
                    .unwrap();
                assert_eq!(
                    conn.portals[slot].params.readable(),
                    retained.as_ref().unwrap()
                );
                assert_eq!(
                    first_row(&message(
                        &mut conn,
                        &mut engine,
                        wire::FMSG_EXECUTE,
                        b"result\0\0\0\0\0",
                        false
                    )),
                    "10"
                );
            } else {
                assert!(conn.pending_bind.is_none());
                assert!(
                    conn.portals
                        .iter()
                        .all(|portal| !portal.active && portal.params.is_empty())
                );
                assert!(!conn.parked);
                assert!(!conn.guc.seq_session().retains_statement_effects());
            }
            if !cancel {
                message(&mut conn, &mut engine, wire::FMSG_SYNC, &[], false);
            }
            assert_eq!(conn.txn.mode, crate::sql::txn::TxnMode::Idle);
            assert!(!conn.guc.seq_session().retains_statement_effects());
            assert_eq!(
                first_row(&query(
                    &mut observer,
                    &mut engine,
                    "SELECT last_value FROM bind_wait_checks"
                )),
                "1"
            );
            assert_eq!(
                first_row(&query(
                    &mut observer,
                    &mut engine,
                    "SELECT hits FROM bind_wait_gate"
                )),
                if cancel || timeout { "0" } else { "1" }
            );
            if cancel || timeout {
                assert!(has_message(
                    &message(
                        &mut conn,
                        &mut engine,
                        wire::FMSG_BIND,
                        &bind_payload("result", &[Some(b"20")]),
                        false
                    ),
                    wire::MSG_BIND_COMPLETE
                ));
                assert_eq!(
                    first_row(&query(
                        &mut observer,
                        &mut engine,
                        "SELECT last_value FROM bind_wait_checks"
                    )),
                    "2"
                );
                assert_eq!(
                    first_row(&message(
                        &mut conn,
                        &mut engine,
                        wire::FMSG_EXECUTE,
                        b"result\0\0\0\0\0",
                        false
                    )),
                    "21"
                );
                message(&mut conn, &mut engine, wire::FMSG_SYNC, &[], false);
                assert_eq!(
                    first_row(&query(
                        &mut observer,
                        &mut engine,
                        "SELECT hits FROM bind_wait_gate"
                    )),
                    "1"
                );
            }
        }
    }
}

#[test]
fn generic_bind_inputs_are_shared_and_pinned_by_old_portals_after_statement_close() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let config = config("bind-generic-inputs", mode);
        let mut budget = crate::sql::test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut conn = Conn::new(&config, &mut budget).unwrap();
        let mut observer = Conn::new(&config, &mut budget).unwrap();
        conn.id = 1;
        observer.id = 2;
        conn.enter_ready_for_tests(&engine);
        observer.enter_ready_for_tests(&engine);
        query(
            &mut conn,
            &mut engine,
            "CREATE SEQUENCE bind_generic_checks; CREATE TYPE bind_generic_choice AS ENUM('a'); \
             CREATE DOMAIN bind_generic_checked AS bind_generic_choice CHECK(nextval('bind_generic_checks')>0); \
             CREATE FUNCTION bind_generic() RETURNS bind_generic_checked[] LANGUAGE sql AS \
             $$SELECT '{a}'::bind_generic_checked[]$$; ALTER SEQUENCE bind_generic_checks RESTART WITH 1; BEGIN",
        );
        parse(&mut conn, &mut engine, "SELECT bind_generic()", &[]);
        for portal in ["first", "second"] {
            assert!(has_message(
                &message(
                    &mut conn,
                    &mut engine,
                    wire::FMSG_BIND,
                    &bind_payload(portal, &[]),
                    false
                ),
                wire::MSG_BIND_COMPLETE
            ));
            assert_eq!(
                first_row(&query(
                    &mut observer,
                    &mut engine,
                    "SELECT last_value FROM bind_generic_checks"
                )),
                "1"
            );
        }
        query(&mut conn, &mut engine, "DISCARD PLANS");
        message(&mut conn, &mut engine, wire::FMSG_CLOSE, b"Pfirst\0", false);
        message(
            &mut conn,
            &mut engine,
            wire::FMSG_BIND,
            &bind_payload("fresh", &[]),
            false,
        );
        assert_eq!(
            first_row(&query(
                &mut observer,
                &mut engine,
                "SELECT last_value FROM bind_generic_checks"
            )),
            "2"
        );
        message(&mut conn, &mut engine, wire::FMSG_CLOSE, b"Sbound\0", false);
        parse(&mut conn, &mut engine, "SELECT 99", &[]);
        for (execute, close) in [
            (&b"second\0\0\0\0\0"[..], &b"Psecond\0"[..]),
            (&b"fresh\0\0\0\0\0"[..], &b"Pfresh\0"[..]),
        ] {
            assert_eq!(
                first_row(&message(
                    &mut conn,
                    &mut engine,
                    wire::FMSG_EXECUTE,
                    execute,
                    false
                )),
                "{a}"
            );
            message(&mut conn, &mut engine, wire::FMSG_CLOSE, close, false);
        }
        message(
            &mut conn,
            &mut engine,
            wire::FMSG_BIND,
            &bind_payload("result", &[]),
            false,
        );
        assert_eq!(
            first_row(&message(
                &mut conn,
                &mut engine,
                wire::FMSG_EXECUTE,
                b"result\0\0\0\0\0",
                false
            )),
            "99"
        );
        message(&mut conn, &mut engine, wire::FMSG_SYNC, &[], false);
        assert_eq!(
            first_row(&query(
                &mut observer,
                &mut engine,
                "SELECT last_value FROM bind_generic_checks"
            )),
            "2"
        );
        query(&mut conn, &mut engine, "COMMIT");
    }
}

#[test]
fn custom_bind_parameters_prune_unreached_inputs_before_checking_function_bodies() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let config = config("bind-parameter-pruning", mode);
        let mut budget = crate::sql::test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut conn = Conn::new(&config, &mut budget).unwrap();
        let mut observer = Conn::new(&config, &mut budget).unwrap();
        conn.id = 1;
        observer.id = 2;
        conn.enter_ready_for_tests(&engine);
        observer.enter_ready_for_tests(&engine);
        query(
            &mut conn,
            &mut engine,
            "CREATE SEQUENCE bind_branch_checks; CREATE TYPE bind_branch_choice AS ENUM('a'); \
             CREATE DOMAIN bind_branch_checked AS bind_branch_choice CHECK(nextval('bind_branch_checks')>0); \
             CREATE FUNCTION bind_branch() RETURNS integer LANGUAGE sql AS \
             $$SELECT cardinality('{a}'::bind_branch_checked[])$$; \
             CREATE FUNCTION bind_branch_strict(integer) RETURNS integer LANGUAGE sql STRICT AS \
             $$SELECT $1+cardinality('{a}'::bind_branch_checked[])$$; \
             ALTER SEQUENCE bind_branch_checks RESTART WITH 1",
        );
        for (sql, oid, inputs) in [
            (
                "SELECT CASE WHEN $1=0 THEN bind_branch() ELSE 17 END",
                crate::sql::types::oid::INT4,
                [Some(&b"1"[..]), Some(&b"0"[..])],
            ),
            (
                "SELECT CASE WHEN $1='go' THEN bind_branch() ELSE 17 END",
                crate::sql::types::oid::TEXT,
                [Some(&b"stop"[..]), Some(&b"go"[..])],
            ),
        ] {
            parse(&mut conn, &mut engine, sql, &[oid]);
            for (parameter, expected_row, expected_checks) in
                [(inputs[0], "17", "f"), (inputs[1], "1", "t")]
            {
                message(
                    &mut conn,
                    &mut engine,
                    wire::FMSG_BIND,
                    &bind_payload("result", &[parameter]),
                    false,
                );
                assert_eq!(
                    first_row(&query(
                        &mut observer,
                        &mut engine,
                        "SELECT is_called FROM bind_branch_checks"
                    )),
                    expected_checks,
                    "mode {mode:?}: {sql}, parameter {parameter:?}"
                );
                assert_eq!(
                    first_row(&message(
                        &mut conn,
                        &mut engine,
                        wire::FMSG_EXECUTE,
                        b"result\0\0\0\0\0",
                        false
                    )),
                    expected_row
                );
                message(&mut conn, &mut engine, wire::FMSG_SYNC, &[], false);
            }
            message(&mut conn, &mut engine, wire::FMSG_CLOSE, b"Sbound\0", false);
            query(
                &mut conn,
                &mut engine,
                "ALTER SEQUENCE bind_branch_checks RESTART WITH 1",
            );
        }
        parse(
            &mut conn,
            &mut engine,
            "SELECT bind_branch_strict($1)",
            &[crate::sql::types::oid::INT4],
        );
        message(
            &mut conn,
            &mut engine,
            wire::FMSG_BIND,
            &bind_payload("result", &[None]),
            false,
        );
        assert_eq!(
            first_row(&query(
                &mut observer,
                &mut engine,
                "SELECT is_called FROM bind_branch_checks"
            )),
            "f"
        );
        assert_eq!(
            first_row(&message(
                &mut conn,
                &mut engine,
                wire::FMSG_EXECUTE,
                b"result\0\0\0\0\0",
                false
            )),
            "NULL"
        );
        message(&mut conn, &mut engine, wire::FMSG_SYNC, &[], false);
    }
}

#[test]
fn concurrent_index_commands_publish_their_real_states_before_sync() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let config = config("concurrent-index-phases", mode);
        let mut budget = crate::sql::test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut builder = Conn::new(&config, &mut budget).unwrap();
        let mut observer = Conn::new(&config, &mut budget).unwrap();
        builder.id = 1;
        observer.id = 2;
        builder.enter_ready_for_tests(&engine);
        observer.enter_ready_for_tests(&engine);
        query(
            &mut builder,
            &mut engine,
            "CREATE TABLE phase_index_rows(id integer); INSERT INTO phase_index_rows VALUES(1),(2)",
        );
        let flags = "SELECT i.indisvalid,i.indisready,i.indislive FROM pg_index i \
            JOIN pg_class c ON c.oid=i.indexrelid WHERE c.relname='phase_index_rows_idx'";
        bind_command(
            &mut builder,
            &mut engine,
            "CREATE INDEX CONCURRENTLY phase_index_rows_idx ON phase_index_rows(id)",
        );
        let response = message(
            &mut builder,
            &mut engine,
            wire::FMSG_EXECUTE,
            &[0; 5],
            false,
        );
        assert_eq!(command_completion_in(&response), Some("CREATE INDEX"));
        assert_eq!(
            first_row(&query(&mut observer, &mut engine, flags)),
            "f|t|t"
        );
        message(&mut builder, &mut engine, wire::FMSG_SYNC, &[], false);
        assert_eq!(
            first_row(&query(&mut observer, &mut engine, flags)),
            "t|t|t"
        );

        bind_command(
            &mut builder,
            &mut engine,
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS phase_index_rows_idx ON phase_index_rows(id)",
        );
        message(
            &mut builder,
            &mut engine,
            wire::FMSG_EXECUTE,
            &[0; 5],
            false,
        );
        assert_eq!(builder.txn.mode, crate::sql::txn::TxnMode::Idle);
        message(&mut builder, &mut engine, wire::FMSG_SYNC, &[], false);
        bind_command(
            &mut builder,
            &mut engine,
            "DROP INDEX CONCURRENTLY phase_index_rows_idx",
        );
        let response = message(
            &mut builder,
            &mut engine,
            wire::FMSG_EXECUTE,
            &[0; 5],
            false,
        );
        assert_eq!(command_completion_in(&response), Some("DROP INDEX"));
        assert_eq!(
            first_row(&query(&mut observer, &mut engine, flags)),
            "f|f|f"
        );
        message(&mut builder, &mut engine, wire::FMSG_SYNC, &[], false);
        assert_eq!(
            first_row(&query(
                &mut observer,
                &mut engine,
                "SELECT count(*) FROM pg_class WHERE relname='phase_index_rows_idx'"
            )),
            "0"
        );
        bind_command(
            &mut builder,
            &mut engine,
            "DROP INDEX CONCURRENTLY IF EXISTS phase_index_rows_idx",
        );
        message(
            &mut builder,
            &mut engine,
            wire::FMSG_EXECUTE,
            &[0; 5],
            false,
        );
        assert_eq!(builder.txn.mode, crate::sql::txn::TxnMode::Idle);
        message(&mut builder, &mut engine, wire::FMSG_SYNC, &[], false);

        query(
            &mut builder,
            &mut engine,
            "CREATE TEMP TABLE phase_index_temp(id integer)",
        );
        bind_command(
            &mut builder,
            &mut engine,
            "CREATE INDEX CONCURRENTLY phase_index_temp_idx ON phase_index_temp(id)",
        );
        message(
            &mut builder,
            &mut engine,
            wire::FMSG_EXECUTE,
            &[0; 5],
            false,
        );
        assert_eq!(builder.txn.mode, crate::sql::txn::TxnMode::Idle);
        message(&mut builder, &mut engine, wire::FMSG_SYNC, &[], false);
        bind_command(
            &mut builder,
            &mut engine,
            "DROP INDEX CONCURRENTLY phase_index_temp_idx",
        );
        message(
            &mut builder,
            &mut engine,
            wire::FMSG_EXECUTE,
            &[0; 5],
            false,
        );
        assert_eq!(builder.txn.mode, crate::sql::txn::TxnMode::Idle);
        message(&mut builder, &mut engine, wire::FMSG_SYNC, &[], false);
    }
}

#[test]
fn concurrent_reindex_replaces_backing_indexes_and_publishes_before_sync() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let config = config("concurrent-reindex-identities", mode);
        let mut budget = crate::sql::test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut builder = Conn::new(&config, &mut budget).unwrap();
        let mut observer = Conn::new(&config, &mut budget).unwrap();
        builder.id = 1;
        observer.id = 2;
        builder.enter_ready_for_tests(&engine);
        observer.enter_ready_for_tests(&engine);
        query(
            &mut builder,
            &mut engine,
            "CREATE TABLE reindex_identity_rows(id integer PRIMARY KEY, code integer UNIQUE, value text); \
             CREATE INDEX reindex_identity_value ON reindex_identity_rows(value); \
             INSERT INTO reindex_identity_rows VALUES(1,11,'one'); \
             COMMENT ON CONSTRAINT reindex_identity_rows_pkey ON reindex_identity_rows IS 'key comment'; \
             COMMENT ON INDEX reindex_identity_rows_pkey IS 'index comment'",
        );
        let old_primary = first_row(&query(
            &mut observer,
            &mut engine,
            "SELECT oid FROM pg_class WHERE relname='reindex_identity_rows_pkey'",
        ));
        let constraint = first_row(&query(
            &mut observer,
            &mut engine,
            "SELECT oid FROM pg_constraint WHERE conname='reindex_identity_rows_pkey'",
        ));
        let old_toast = first_row(&query(
            &mut observer,
            &mut engine,
            "SELECT i.indexrelid FROM pg_index i JOIN pg_class c ON c.oid=i.indrelid \
             JOIN pg_class owner ON owner.reltoastrelid=c.oid WHERE owner.relname='reindex_identity_rows'",
        ));
        bind_command(
            &mut builder,
            &mut engine,
            "REINDEX TABLE CONCURRENTLY reindex_identity_rows",
        );
        let response = message(
            &mut builder,
            &mut engine,
            wire::FMSG_EXECUTE,
            &[0; 5],
            false,
        );
        assert_eq!(command_completion_in(&response), Some("REINDEX"));
        assert_eq!(
            first_row(&query(
                &mut observer,
                &mut engine,
                &format!("SELECT count(*) FROM pg_class WHERE oid IN ({old_primary},{old_toast})")
            )),
            "0"
        );
        assert_eq!(
            first_row(&query(
                &mut observer,
                &mut engine,
                "SELECT count(*) FROM pg_class WHERE relname LIKE 'reindex_identity%_ccnew%' \
             OR relname LIKE 'reindex_identity%_ccold%'"
            )),
            "0"
        );
        assert_eq!(
            first_row(&query(
                &mut observer,
                &mut engine,
                &format!(
                    "SELECT oid,conindid <> {old_primary},obj_description(oid,'pg_constraint') \
             FROM pg_constraint WHERE conname='reindex_identity_rows_pkey'"
                )
            )),
            format!("{constraint}|t|key comment")
        );
        assert_eq!(
            first_row(&query(
                &mut observer,
                &mut engine,
                "SELECT obj_description(oid,'pg_class') FROM pg_class WHERE relname='reindex_identity_rows_pkey'"
            )),
            "index comment"
        );
        assert_eq!(
            first_row(&query(
                &mut observer,
                &mut engine,
                "SELECT count(*) FROM pg_index i JOIN pg_class c ON c.oid=i.indrelid \
             WHERE c.relname='reindex_identity_rows' AND i.indisvalid AND i.indisready AND i.indislive"
            )),
            "3"
        );
        assert_eq!(builder.txn.mode, crate::sql::txn::TxnMode::Implicit);
        message(&mut builder, &mut engine, wire::FMSG_SYNC, &[], false);
        assert_eq!(
            first_row(&query(
                &mut observer,
                &mut engine,
                "SELECT id,code,value FROM reindex_identity_rows"
            )),
            "1|11|one"
        );
        query(
            &mut builder,
            &mut engine,
            "CREATE TABLE reindex_deferred_rows(id integer UNIQUE DEFERRABLE INITIALLY DEFERRED); \
             INSERT INTO reindex_deferred_rows VALUES(1)",
        );
        query(
            &mut builder,
            &mut engine,
            "REINDEX INDEX CONCURRENTLY reindex_deferred_rows_id_key",
        );
        query(
            &mut builder,
            &mut engine,
            "BEGIN; INSERT INTO reindex_deferred_rows VALUES(2),(2); \
             DELETE FROM reindex_deferred_rows WHERE id=2; COMMIT",
        );
        assert_eq!(
            first_row(&query(
                &mut observer,
                &mut engine,
                "SELECT i.indimmediate FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid \
             WHERE c.relname='reindex_deferred_rows_id_key'"
            )),
            "f"
        );
    }
}

#[test]
fn reindex_tablespace_moves_user_indexes_and_preserves_toast_placement() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let config = config("reindex-toast-tablespace", mode);
        let mut budget = crate::sql::test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = Conn::new(&config, &mut budget).unwrap();
        session.id = 1;
        session.enter_ready_for_tests(&engine);
        query(
            &mut session,
            &mut engine,
            "CREATE TABLESPACE reindex_first_space LOCATION '/object/reindex-first'",
        );
        query(
            &mut session,
            &mut engine,
            "CREATE TABLESPACE reindex_second_space LOCATION '/object/reindex-second'",
        );
        query(
            &mut session,
            &mut engine,
            "CREATE TABLE reindex_space_rows(id integer PRIMARY KEY, value text); \
             INSERT INTO reindex_space_rows VALUES(1,'retained')",
        );
        let toast_before = first_row(&query(
            &mut session,
            &mut engine,
            "SELECT i.indexrelid FROM pg_index i JOIN pg_class toast ON toast.oid=i.indrelid \
             JOIN pg_class parent ON parent.reltoastrelid=toast.oid WHERE parent.relname='reindex_space_rows'",
        ));
        for (tablespace, concurrent) in [
            ("reindex_first_space", false),
            ("reindex_second_space", true),
        ] {
            query(
                &mut session,
                &mut engine,
                &format!(
                    "REINDEX (TABLESPACE {tablespace}) TABLE {}reindex_space_rows",
                    if concurrent { "CONCURRENTLY " } else { "" }
                ),
            );
            assert_eq!(
                first_row(&query(
                    &mut session,
                    &mut engine,
                    "SELECT count(*),count(DISTINCT c.oid),count(*) FILTER (WHERE i.indisprimary) \
                     FROM pg_class c JOIN pg_index i ON i.indexrelid=c.oid \
                     WHERE i.indrelid='reindex_space_rows'::regclass"
                )),
                "1|1|1",
                "{mode:?}: each materialized key has one relation identity"
            );
            assert_eq!(
                first_row(&query(
                    &mut session,
                    &mut engine,
                    "SELECT space.spcname FROM pg_class idx JOIN pg_tablespace space ON space.oid=idx.reltablespace \
                 WHERE idx.relname='reindex_space_rows_pkey'"
                )),
                tablespace
            );
            assert_eq!(
                first_row(&query(
                    &mut session,
                    &mut engine,
                    "SELECT idx.reltablespace FROM pg_index i JOIN pg_class idx ON idx.oid=i.indexrelid \
                 JOIN pg_class toast ON toast.oid=i.indrelid JOIN pg_class parent ON parent.reltoastrelid=toast.oid \
                 WHERE parent.relname='reindex_space_rows'"
                )),
                "0"
            );
            assert_eq!(
                first_row(&query(
                    &mut session,
                    &mut engine,
                    &format!(
                        "SELECT i.indexrelid={toast_before} FROM pg_index i JOIN pg_class toast ON toast.oid=i.indrelid \
                 JOIN pg_class parent ON parent.reltoastrelid=toast.oid WHERE parent.relname='reindex_space_rows'"
                    )
                )),
                if concurrent { "f" } else { "t" }
            );
        }
        let toast_name = first_row(&query(
            &mut session,
            &mut engine,
            "SELECT idx.relname FROM pg_index i JOIN pg_class idx ON idx.oid=i.indexrelid \
             JOIN pg_class toast ON toast.oid=i.indrelid JOIN pg_class parent ON parent.reltoastrelid=toast.oid \
             WHERE parent.relname='reindex_space_rows'",
        ));
        for concurrent in [false, true] {
            let sql = format!(
                "REINDEX (TABLESPACE reindex_first_space) INDEX {}pg_toast.{toast_name}",
                if concurrent { "CONCURRENTLY " } else { "" }
            );
            let mut payload = sql.into_bytes();
            payload.push(0);
            let response = message(&mut session, &mut engine, wire::FMSG_QUERY, &payload, true);
            assert!(response.windows(5).any(|bytes| bytes == b"0A000"));
            assert!(String::from_utf8_lossy(&response).contains("cannot move system relation"));
        }
        query(
            &mut session,
            &mut engine,
            "CREATE TEMP TABLE reindex_temp_space(id integer PRIMARY KEY, value text)",
        );
        let temporary_index = first_row(&query(
            &mut session,
            &mut engine,
            "SELECT conindid FROM pg_constraint WHERE conname='reindex_temp_space_pkey'",
        ));
        query(
            &mut session,
            &mut engine,
            "REINDEX (TABLESPACE reindex_first_space) TABLE CONCURRENTLY reindex_temp_space",
        );
        assert_eq!(
            first_row(&query(
                &mut session,
                &mut engine,
                "SELECT conindid FROM pg_constraint WHERE conname='reindex_temp_space_pkey'"
            )),
            temporary_index
        );
        assert_eq!(
            first_row(&query(
                &mut session,
                &mut engine,
                "SELECT idx.reltablespace FROM pg_index i JOIN pg_class idx ON idx.oid=i.indexrelid \
             JOIN pg_class toast ON toast.oid=i.indrelid JOIN pg_class parent ON parent.reltoastrelid=toast.oid \
             WHERE parent.relname='reindex_temp_space'"
            )),
            "0"
        );
        assert_eq!(
            first_row(&query(
                &mut session,
                &mut engine,
                "SELECT space.spcname FROM pg_class idx JOIN pg_tablespace space ON space.oid=idx.reltablespace \
             WHERE idx.relname='reindex_temp_space_pkey'"
            )),
            "reindex_first_space"
        );
    }
}

#[test]
fn concurrent_reindex_cancel_keeps_the_published_replacement_and_old_constraint() {
    for (key, immediate) in [
        ("PRIMARY KEY", "t"),
        ("PRIMARY KEY DEFERRABLE INITIALLY DEFERRED", "f"),
    ] {
        let config = config(
            &format!("concurrent-reindex-cancel-{immediate}"),
            crate::config::TransactionMode::Mvcc,
        );
        let mut budget = crate::sql::test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut builder = Conn::new(&config, &mut budget).unwrap();
        let mut writer = Conn::new(&config, &mut budget).unwrap();
        let mut observer = Conn::new(&config, &mut budget).unwrap();
        builder.id = 1;
        writer.id = 2;
        observer.id = 3;
        builder.enter_ready_for_tests(&engine);
        writer.enter_ready_for_tests(&engine);
        observer.enter_ready_for_tests(&engine);
        query(
            &mut builder,
            &mut engine,
            &format!(
                "CREATE TABLE reindex_cancel_rows(id integer {key}); INSERT INTO reindex_cancel_rows VALUES(1)"
            ),
        );
        let old = first_row(&query(
            &mut observer,
            &mut engine,
            "SELECT conindid FROM pg_constraint WHERE conname='reindex_cancel_rows_pkey'",
        ));
        query(
            &mut writer,
            &mut engine,
            "BEGIN; INSERT INTO reindex_cancel_rows VALUES(2)",
        );
        bind_command(
            &mut builder,
            &mut engine,
            "REINDEX INDEX CONCURRENTLY reindex_cancel_rows_pkey",
        );
        builder.send.clear();
        assert!(builder.recv.append(&frontend(wire::FMSG_EXECUTE, &[0; 5])));
        let step = crate::mem::guard::forbid_alloc(|| builder.process_message(&mut engine));
        assert!(
            matches!(step, Step::Parked),
            "{step:?}: {}",
            String::from_utf8_lossy(builder.send.readable())
        );
        assert!(
            builder.txn.staged_commit().is_none(),
            "the writer wait must follow durable replacement publication"
        );
        assert_eq!(
            first_row(&query(
                &mut observer,
                &mut engine,
                "SELECT i.indisvalid,i.indisready,i.indislive,i.indimmediate FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid \
         WHERE c.relname='reindex_cancel_rows_pkey_ccnew'"
            )),
            format!("f|f|t|{immediate}")
        );
        assert_eq!(
            first_row(&query(
                &mut observer,
                &mut engine,
                "SELECT conindid FROM pg_constraint WHERE conname='reindex_cancel_rows_pkey'"
            )),
            old
        );
        assert!(crate::mem::guard::forbid_alloc(
            || builder.cancel_parked(&mut engine)
        ));
        message(&mut builder, &mut engine, wire::FMSG_SYNC, &[], false);
        query(&mut writer, &mut engine, "COMMIT");
        assert_eq!(
            first_row(&query(
                &mut observer,
                &mut engine,
                "SELECT i.indisvalid,i.indisready,i.indislive FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid \
         WHERE c.relname='reindex_cancel_rows_pkey'"
            )),
            "t|t|t"
        );
        query(
            &mut builder,
            &mut engine,
            "DROP INDEX reindex_cancel_rows_pkey_ccnew",
        );
        query(
            &mut builder,
            &mut engine,
            "REINDEX INDEX CONCURRENTLY reindex_cancel_rows_pkey",
        );
        assert_eq!(
            first_row(&query(
                &mut observer,
                &mut engine,
                "SELECT conindid <> 0 FROM pg_constraint WHERE conname='reindex_cancel_rows_pkey'"
            )),
            "t"
        );
    }
}

#[test]
fn concurrent_reindex_schema_finishes_each_table_before_waiting_on_the_next() {
    let config = config(
        "concurrent-reindex-batch",
        crate::config::TransactionMode::Mvcc,
    );
    let mut budget = crate::sql::test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut builder = Conn::new(&config, &mut budget).unwrap();
    let mut writer = Conn::new(&config, &mut budget).unwrap();
    let mut observer = Conn::new(&config, &mut budget).unwrap();
    builder.id = 1;
    writer.id = 2;
    observer.id = 3;
    builder.enter_ready_for_tests(&engine);
    writer.enter_ready_for_tests(&engine);
    observer.enter_ready_for_tests(&engine);
    query(
        &mut builder,
        &mut engine,
        "CREATE SCHEMA reindex_batch; CREATE TABLE reindex_batch.first_rows(id integer); \
         CREATE TABLE reindex_batch.second_rows(id integer); \
         CREATE INDEX first_idx ON reindex_batch.first_rows(id); \
         CREATE INDEX second_idx ON reindex_batch.second_rows(id)",
    );
    let first_old = first_row(&query(
        &mut observer,
        &mut engine,
        "SELECT oid FROM pg_class WHERE relname='first_idx'",
    ));
    query(
        &mut writer,
        &mut engine,
        "BEGIN; INSERT INTO reindex_batch.second_rows VALUES(1)",
    );
    bind_command(
        &mut builder,
        &mut engine,
        "REINDEX SCHEMA CONCURRENTLY reindex_batch",
    );
    builder.send.clear();
    assert!(builder.recv.append(&frontend(wire::FMSG_EXECUTE, &[0; 5])));
    let step = crate::mem::guard::forbid_alloc(|| builder.process_message(&mut engine));
    assert!(
        matches!(step, Step::Parked),
        "{step:?}: {}",
        String::from_utf8_lossy(builder.send.readable())
    );
    assert_eq!(
        first_row(&query(
            &mut observer,
            &mut engine,
            &format!("SELECT count(*) FROM pg_class WHERE oid={first_old}")
        )),
        "0"
    );
    assert_eq!(
        first_row(&query(
            &mut observer,
            &mut engine,
            "SELECT i.indisvalid,i.indisready,i.indislive FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid \
         WHERE c.relname='second_idx_ccnew'"
        )),
        "f|f|t"
    );
    query(
        &mut observer,
        &mut engine,
        "BEGIN; LOCK TABLE reindex_batch.first_rows IN ACCESS EXCLUSIVE MODE NOWAIT; ROLLBACK",
    );
    query(&mut writer, &mut engine, "COMMIT");
    builder.send.clear();
    let step = crate::mem::guard::forbid_alloc(|| builder.process_message(&mut engine));
    assert!(
        matches!(step, Step::Continue),
        "{step:?}: {}",
        String::from_utf8_lossy(builder.send.readable())
    );
    assert_eq!(
        command_completion_in(builder.send.readable()),
        Some("REINDEX")
    );
    message(&mut builder, &mut engine, wire::FMSG_SYNC, &[], false);
    assert_eq!(
        first_row(&query(
            &mut observer,
            &mut engine,
            "SELECT count(*) FROM pg_class WHERE relname IN ('first_idx_ccnew','first_idx_ccold','second_idx_ccnew','second_idx_ccold')"
        )),
        "0"
    );
}

#[test]
fn concurrent_index_build_waits_for_earlier_writers_and_keeps_published_failures() {
    let config = config(
        "concurrent-index-writer-wait",
        crate::config::TransactionMode::Mvcc,
    );
    let mut budget = crate::sql::test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut builder = Conn::new(&config, &mut budget).unwrap();
    let mut writer = Conn::new(&config, &mut budget).unwrap();
    let mut observer = Conn::new(&config, &mut budget).unwrap();
    builder.id = 1;
    writer.id = 2;
    observer.id = 3;
    builder.enter_ready_for_tests(&engine);
    writer.enter_ready_for_tests(&engine);
    observer.enter_ready_for_tests(&engine);
    query(
        &mut builder,
        &mut engine,
        "CREATE TABLE phase_wait_rows(id integer)",
    );
    let flags = "SELECT i.indisvalid,i.indisready,i.indislive FROM pg_index i \
        JOIN pg_class c ON c.oid=i.indexrelid WHERE c.relname='phase_wait_idx'";
    for cancel in [true, false] {
        query(
            &mut writer,
            &mut engine,
            "BEGIN; INSERT INTO phase_wait_rows VALUES(1)",
        );
        bind_command(
            &mut builder,
            &mut engine,
            "CREATE UNIQUE INDEX CONCURRENTLY phase_wait_idx ON phase_wait_rows(id)",
        );
        builder.send.clear();
        assert!(builder.recv.append(&frontend(wire::FMSG_EXECUTE, &[0; 5])));
        let step = crate::mem::guard::forbid_alloc(|| builder.process_message(&mut engine));
        assert!(
            matches!(step, Step::Parked),
            "{step:?}: {}",
            String::from_utf8_lossy(builder.send.readable())
        );
        assert_eq!(
            first_row(&query(&mut observer, &mut engine, flags)),
            "f|f|t"
        );
        query(
            &mut observer,
            &mut engine,
            "INSERT INTO phase_wait_rows VALUES(2)",
        );
        if cancel {
            assert!(crate::mem::guard::forbid_alloc(
                || builder.cancel_parked(&mut engine)
            ));
            assert!(
                builder
                    .send
                    .readable()
                    .windows(5)
                    .any(|bytes| bytes == b"57014")
            );
            message(&mut builder, &mut engine, wire::FMSG_SYNC, &[], false);
            assert_eq!(
                first_row(&query(&mut observer, &mut engine, flags)),
                "f|f|t"
            );
        } else {
            query(&mut writer, &mut engine, "COMMIT");
            let step = crate::mem::guard::forbid_alloc(|| builder.process_message(&mut engine));
            assert!(
                matches!(step, Step::Continue),
                "{step:?}: {}",
                String::from_utf8_lossy(builder.send.readable())
            );
            assert!(
                !has_message(builder.send.readable(), wire::MSG_ERROR_RESPONSE),
                "{}",
                String::from_utf8_lossy(builder.send.readable())
            );
            message(&mut builder, &mut engine, wire::FMSG_SYNC, &[], false);
            assert_eq!(
                first_row(&query(&mut observer, &mut engine, flags)),
                "t|t|t"
            );
            assert_eq!(
                first_row(&query(
                    &mut observer,
                    &mut engine,
                    "SELECT count(*) FROM phase_wait_rows WHERE id IN(1,2)"
                )),
                "2"
            );
        }
        query(&mut writer, &mut engine, "ROLLBACK");
        query(
            &mut builder,
            &mut engine,
            "DROP INDEX CONCURRENTLY phase_wait_idx",
        );
        query(&mut builder, &mut engine, "TRUNCATE phase_wait_rows");
    }
    query(
        &mut builder,
        &mut engine,
        "INSERT INTO phase_wait_rows VALUES(1),(1)",
    );
    bind_command(
        &mut builder,
        &mut engine,
        "CREATE UNIQUE INDEX CONCURRENTLY phase_wait_idx ON phase_wait_rows(id)",
    );
    let response = message(&mut builder, &mut engine, wire::FMSG_EXECUTE, &[0; 5], true);
    assert!(
        response.windows(5).any(|bytes| bytes == b"23505"),
        "{}",
        String::from_utf8_lossy(&response)
    );
    message(&mut builder, &mut engine, wire::FMSG_SYNC, &[], false);
    assert_eq!(
        first_row(&query(&mut observer, &mut engine, flags)),
        "f|f|t"
    );
    query(
        &mut writer,
        &mut engine,
        "INSERT INTO phase_wait_rows VALUES(1)",
    );
    assert_eq!(
        first_row(&query(
            &mut observer,
            &mut engine,
            "SELECT count(*) FROM phase_wait_rows"
        )),
        "3"
    );
    query(
        &mut builder,
        &mut engine,
        "DROP INDEX CONCURRENTLY phase_wait_idx",
    );
}

#[test]
#[cfg(not(miri))]
fn concurrent_reindex_receipts_preserve_each_committed_prefix_after_cold_restart() {
    use crate::sql::IndexCommitPhaseForTests as CommitPhase;
    for stop in [
        None,
        Some(CommitPhase::ReindexRegistered),
        Some(CommitPhase::ReindexBuilt),
        Some(CommitPhase::ReindexSwapped),
        Some(CommitPhase::ReindexDead),
    ] {
        let label = match stop {
            None => "complete",
            Some(CommitPhase::ReindexRegistered) => "registered",
            Some(CommitPhase::ReindexBuilt) => "built",
            Some(CommitPhase::ReindexSwapped) => "swapped",
            Some(CommitPhase::ReindexDead) => "dead",
            Some(other) => panic!("unexpected stop phase {other:?}"),
        };
        let mut config = config(
            &format!("reindex-receipt-prefix-{stop:?}"),
            crate::config::TransactionMode::Mvcc,
        );
        config.object_store_on = true;
        config.object_store_bucket = format!("reindex-prefix-{}-{label}", std::process::id());
        config.wal_upload = true;
        config.wal_upload_sync = true;
        config.wal_upload_buffer_bytes = 1 << 20;
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let server = crate::sql::HttpObjectServer::start(&mut config);
        let mut budget = crate::sql::test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut builder = Conn::new(&config, &mut budget).unwrap();
        let mut observer = Conn::new(&config, &mut budget).unwrap();
        builder.id = 1;
        observer.id = 2;
        builder.enter_ready_for_tests(&engine);
        observer.enter_ready_for_tests(&engine);
        query(
            &mut builder,
            &mut engine,
            "CREATE TABLE receipt_reindex_rows(id integer PRIMARY KEY, payload text); \
             INSERT INTO receipt_reindex_rows VALUES(1,'one'),(2,'two'); \
             COMMENT ON CONSTRAINT receipt_reindex_rows_pkey ON receipt_reindex_rows IS 'constraint survives'; \
             COMMENT ON INDEX receipt_reindex_rows_pkey IS 'index survives'; \
             CREATE SEQUENCE receipt_reindex_start; CREATE SEQUENCE receipt_reindex_end; \
             CREATE FUNCTION receipt_reindex_started() RETURNS event_trigger LANGUAGE plpgsql AS \
             $$BEGIN PERFORM nextval('receipt_reindex_start'); END$$; \
             CREATE FUNCTION receipt_reindex_finished() RETURNS event_trigger LANGUAGE plpgsql AS \
             $$BEGIN PERFORM nextval('receipt_reindex_end'); END$$; \
             CREATE EVENT TRIGGER receipt_reindex_start_trigger ON ddl_command_start WHEN TAG IN ('REINDEX') \
             EXECUTE FUNCTION receipt_reindex_started(); \
             CREATE EVENT TRIGGER receipt_reindex_end_trigger ON ddl_command_end WHEN TAG IN ('REINDEX') \
             EXECUTE FUNCTION receipt_reindex_finished()",
        );
        let original = first_row(&query(
            &mut observer,
            &mut engine,
            "SELECT conindid FROM pg_constraint WHERE conname='receipt_reindex_rows_pkey'",
        ));
        let constraint = first_row(&query(
            &mut observer,
            &mut engine,
            "SELECT oid FROM pg_constraint WHERE conname='receipt_reindex_rows_pkey'",
        ));
        let toast = first_row(&query(
            &mut observer,
            &mut engine,
            "SELECT reltoastrelid FROM pg_class WHERE relname='receipt_reindex_rows'",
        ));
        if stop.is_none() {
            query(
                &mut builder,
                &mut engine,
                "CREATE ROLE receipt_reindex_maintainer; \
                 GRANT MAINTAIN ON receipt_reindex_rows TO receipt_reindex_maintainer; \
                 GRANT USAGE ON SEQUENCE receipt_reindex_start,receipt_reindex_end TO receipt_reindex_maintainer; \
                 SET ROLE receipt_reindex_maintainer",
            );
        }
        engine.commit_wal().unwrap();
        engine.enable_async_durability();
        bind_command(
            &mut builder,
            &mut engine,
            "REINDEX TABLE CONCURRENTLY receipt_reindex_rows",
        );
        builder.send.clear();
        assert!(builder.recv.append(&frontend(wire::FMSG_EXECUTE, &[0; 5])));
        let mut cancelled = false;
        let mut completed = false;
        for _ in 0..16 {
            let responses = server.hold_upload_responses();
            let step = crate::mem::guard::forbid_alloc(|| builder.process_message(&mut engine));
            if matches!(step, Step::Continue) {
                completed = true;
                break;
            }
            assert!(
                matches!(step, Step::Parked),
                "{stop:?}: {step:?}: {}",
                String::from_utf8_lossy(builder.send.readable())
            );
            let target = builder
                .txn
                .staged_commit()
                .expect("phase must await durable object receipt")
                .target;
            let crate::sql::txn::CommitTarget::ConcurrentIndex { phase, .. } = target else {
                panic!("wrong durable phase owner: {target:?}");
            };
            for _ in 0..2 {
                let repeated =
                    crate::mem::guard::forbid_alloc(|| builder.process_message(&mut engine));
                assert!(
                    matches!(repeated, Step::Parked),
                    "{stop:?}: {repeated:?}: {}",
                    String::from_utf8_lossy(builder.send.readable())
                );
                assert_eq!(builder.txn.staged_commit().unwrap().target, target);
                assert!(!has_message(
                    builder.send.readable(),
                    wire::MSG_COMMAND_COMPLETE
                ));
                assert!(!has_message(
                    builder.send.readable(),
                    wire::MSG_ERROR_RESPONSE
                ));
            }
            if stop == Some(phase) {
                assert!(!cancelled);
                assert!(!crate::mem::guard::forbid_alloc(
                    || builder.cancel_parked(&mut engine)
                ));
                assert!(builder.txn.staged_commit().is_some());
                cancelled = true;
            }
            drop(responses);
            engine.commit_wal().unwrap();
        }
        assert!(
            completed,
            "REINDEX exceeded its bounded phase count: {stop:?}"
        );
        assert_eq!(cancelled, stop.is_some());
        if cancelled {
            assert!(
                has_message(builder.send.readable(), wire::MSG_ERROR_RESPONSE),
                "{}",
                String::from_utf8_lossy(builder.send.readable())
            );
            assert!(
                builder
                    .send
                    .readable()
                    .windows(5)
                    .any(|bytes| bytes == b"57014")
            );
            assert!(!has_message(
                builder.send.readable(),
                wire::MSG_COMMAND_COMPLETE
            ));
            message(&mut builder, &mut engine, wire::FMSG_SYNC, &[], false);
        } else {
            assert_eq!(
                command_completion_in(builder.send.readable()),
                Some("REINDEX")
            );
            assert!(!has_message(
                builder.send.readable(),
                wire::MSG_ERROR_RESPONSE
            ));
            builder.send.clear();
            assert!(builder.recv.append(&frontend(wire::FMSG_SYNC, &[])));
            let mut synced = false;
            for _ in 0..3 {
                let step = crate::mem::guard::forbid_alloc(|| builder.process_message(&mut engine));
                if matches!(step, Step::Continue) {
                    synced = true;
                    break;
                }
                assert!(
                    matches!(step, Step::Parked),
                    "{step:?}: {}",
                    String::from_utf8_lossy(builder.send.readable())
                );
                assert!(builder.txn.staged_commit().is_some());
                engine.commit_wal().unwrap();
            }
            assert!(synced);
        }
        assert_eq!(builder.txn.mode, crate::sql::txn::TxnMode::Idle);
        engine.commit_wal().unwrap();
        drop(builder);
        drop(observer);
        drop(engine);
        std::fs::remove_dir_all(&config.data_dir).unwrap();
        let mut recovered_budget = crate::sql::test_engine_budget(&config, 1 << 29);
        let mut recovered = Engine::new(&config, &mut recovered_budget).unwrap();
        let mut verifier = Conn::new(&config, &mut recovered_budget).unwrap();
        verifier.id = 1;
        verifier.enter_ready_for_tests(&recovered);
        assert_eq!(
            first_row(&query(
                &mut verifier,
                &mut recovered,
                "SELECT id,payload FROM receipt_reindex_rows ORDER BY id"
            )),
            "1|one"
        );
        assert_eq!(
            first_row(&query(
                &mut verifier,
                &mut recovered,
                "SELECT count(*) FROM receipt_reindex_rows"
            )),
            "2"
        );
        assert_eq!(
            first_row(&query(
                &mut verifier,
                &mut recovered,
                "SELECT last_value,is_called FROM receipt_reindex_start"
            )),
            "1|t"
        );
        assert_eq!(
            first_row(&query(
                &mut verifier,
                &mut recovered,
                "SELECT last_value,is_called FROM receipt_reindex_end"
            )),
            if cancelled { "1|f" } else { "1|t" }
        );
        assert_eq!(
            first_row(&query(
                &mut verifier,
                &mut recovered,
                "SELECT oid,obj_description(oid,'pg_constraint') FROM pg_constraint WHERE conname='receipt_reindex_rows_pkey'"
            )),
            format!("{constraint}|constraint survives")
        );
        assert_eq!(
            first_row(&query(
                &mut verifier,
                &mut recovered,
                "SELECT reltoastrelid FROM pg_class WHERE relname='receipt_reindex_rows'"
            )),
            toast
        );
        assert_eq!(
            first_row(&query(
                &mut verifier,
                &mut recovered,
                "SELECT obj_description(oid,'pg_class') FROM pg_class WHERE relname='receipt_reindex_rows_pkey'"
            )),
            "index survives"
        );
        assert_eq!(
            first_row(&query(
                &mut verifier,
                &mut recovered,
                "SELECT pg_get_userbyid(relowner) FROM pg_class WHERE relname='receipt_reindex_rows_pkey'"
            )),
            "postgres"
        );
        let swapped = matches!(
            stop,
            None | Some(CommitPhase::ReindexSwapped | CommitPhase::ReindexDead)
        );
        assert_eq!(
            first_row(&query(
                &mut verifier,
                &mut recovered,
                &format!(
                    "SELECT conindid <> {original} FROM pg_constraint WHERE conname='receipt_reindex_rows_pkey'"
                )
            )),
            if swapped { "t" } else { "f" }
        );
        assert_eq!(
            first_row(&query(
                &mut verifier,
                &mut recovered,
                "SELECT i.indisvalid,i.indisready,i.indislive FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid \
             WHERE c.relname='receipt_reindex_rows_pkey'"
            )),
            "t|t|t"
        );
        match stop {
            Some(CommitPhase::ReindexRegistered | CommitPhase::ReindexBuilt) => {
                assert_eq!(
                    first_row(&query(
                        &mut verifier,
                        &mut recovered,
                        "SELECT i.indisvalid,i.indisready,i.indislive FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid \
                     WHERE c.relname='receipt_reindex_rows_pkey_ccnew'"
                    )),
                    if stop == Some(CommitPhase::ReindexBuilt) {
                        "f|t|t"
                    } else {
                        "f|f|t"
                    }
                );
                assert_eq!(
                    first_row(&query(
                        &mut verifier,
                        &mut recovered,
                        "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace \
                     WHERE n.nspname='pg_toast' AND c.relname LIKE '%_ccnew%'"
                    )),
                    "1"
                );
            }
            Some(CommitPhase::ReindexSwapped | CommitPhase::ReindexDead) => {
                assert_eq!(
                    first_row(&query(
                        &mut verifier,
                        &mut recovered,
                        "SELECT i.indisvalid,i.indisready,i.indislive FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid \
                     WHERE c.relname='receipt_reindex_rows_pkey_ccold'"
                    )),
                    if stop == Some(CommitPhase::ReindexDead) {
                        "f|f|f"
                    } else {
                        "f|t|t"
                    }
                );
                assert_eq!(
                    first_row(&query(
                        &mut verifier,
                        &mut recovered,
                        "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace \
                     WHERE n.nspname='pg_toast' AND c.relname LIKE '%_ccold%'"
                    )),
                    "1"
                );
            }
            None => {
                assert_eq!(
                    first_row(&query(
                        &mut verifier,
                        &mut recovered,
                        &format!(
                            "SELECT count(*) FROM pg_class WHERE oid={original} OR relname LIKE '%_ccnew%' OR relname LIKE '%_ccold%'"
                        )
                    )),
                    "0"
                );
            }
            Some(other) => panic!("unexpected stop phase {other:?}"),
        }
        if matches!(
            stop,
            Some(CommitPhase::ReindexRegistered | CommitPhase::ReindexDead)
        ) {
            let suffix = if stop == Some(CommitPhase::ReindexRegistered) {
                "ccnew"
            } else {
                "ccold"
            };
            let leftover = first_row(&query(
                &mut verifier,
                &mut recovered,
                &format!(
                    "SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace \
                 WHERE n.nspname='pg_toast' AND c.relname LIKE '%_{suffix}%'"
                ),
            ));
            for concurrent in [false, true] {
                let mut payload = format!(
                    "REINDEX INDEX {}pg_toast.{leftover}",
                    if concurrent { "CONCURRENTLY " } else { "" }
                )
                .into_bytes();
                payload.push(0);
                let rejected = message(
                    &mut verifier,
                    &mut recovered,
                    wire::FMSG_QUERY,
                    &payload,
                    true,
                );
                assert!(rejected.windows(5).any(|bytes| bytes == b"0A000"));
                assert!(
                    String::from_utf8_lossy(&rejected)
                        .contains("cannot reindex invalid index on TOAST table")
                );
            }
            let rebuilt = query(
                &mut verifier,
                &mut recovered,
                "REINDEX TABLE receipt_reindex_rows",
            );
            let warns = stop == Some(CommitPhase::ReindexRegistered);
            assert_eq!(has_message(&rebuilt, wire::MSG_NOTICE_RESPONSE), warns);
            assert_eq!(rebuilt.windows(5).any(|bytes| bytes == b"0A000"), warns);
            assert_eq!(
                String::from_utf8_lossy(&rebuilt).contains("on TOAST table, skipping"),
                warns
            );
            assert_eq!(
                first_row(&query(
                    &mut verifier,
                    &mut recovered,
                    &format!(
                        "SELECT i.indisvalid,i.indisready,i.indislive FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid \
                 JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='pg_toast' AND c.relname='{leftover}'"
                    )
                )),
                if stop == Some(CommitPhase::ReindexRegistered) {
                    "f|f|t"
                } else {
                    "f|f|f"
                }
            );
        }
        const OWNERS: &str = "SELECT count(*) = 4 AND count(*) FILTER(WHERE x.indislive) = 2 \
            AND count(*) FILTER(WHERE NOT x.indislive) = 2, \
            bool_and(CASE WHEN x.indislive THEN r.rolname = 'receipt_reindex_owner' \
            ELSE r.rolname = 'postgres' END) \
            FROM pg_class t JOIN pg_index x ON x.indrelid = t.oid OR x.indrelid = t.reltoastrelid \
            JOIN pg_class i ON i.oid = x.indexrelid JOIN pg_roles r ON r.oid = i.relowner \
            WHERE t.oid = 'receipt_reindex_rows'::regclass";
        if stop == Some(CommitPhase::ReindexDead) {
            query(
                &mut verifier,
                &mut recovered,
                "CREATE ROLE receipt_reindex_owner",
            );
            query(
                &mut verifier,
                &mut recovered,
                "BEGIN; ALTER TABLE receipt_reindex_rows OWNER TO receipt_reindex_owner",
            );
            assert_eq!(
                first_row(&query(&mut verifier, &mut recovered, OWNERS)),
                "t|t"
            );
            query(&mut verifier, &mut recovered, "ROLLBACK");
            assert_eq!(
                first_row(&query(
                    &mut verifier,
                    &mut recovered,
                    "SELECT count(*) = 4, bool_and(r.rolname = 'postgres') \
                 FROM pg_class t JOIN pg_index x ON x.indrelid = t.oid OR x.indrelid = t.reltoastrelid \
                 JOIN pg_class i ON i.oid = x.indexrelid JOIN pg_roles r ON r.oid = i.relowner \
                 WHERE t.oid = 'receipt_reindex_rows'::regclass"
                )),
                "t|t"
            );
            query(
                &mut verifier,
                &mut recovered,
                "ALTER TABLE receipt_reindex_rows OWNER TO receipt_reindex_owner",
            );
            assert_eq!(
                first_row(&query(&mut verifier, &mut recovered, OWNERS)),
                "t|t"
            );
        }
        recovered.commit_wal().unwrap();
        drop(verifier);
        drop(recovered);
        if stop == Some(CommitPhase::ReindexDead) {
            for checkpoint in [false, true] {
                std::fs::remove_dir_all(&config.data_dir).unwrap();
                let mut cold_budget = crate::sql::test_engine_budget(&config, 1 << 29);
                let mut cold = Engine::new(&config, &mut cold_budget).unwrap();
                let mut verifier = Conn::new(&config, &mut cold_budget).unwrap();
                verifier.id = 1;
                verifier.enter_ready_for_tests(&cold);
                assert_eq!(
                    first_row(&query(&mut verifier, &mut cold, OWNERS)),
                    "t|t",
                    "retired index owners after checkpoint={checkpoint}"
                );
                if !checkpoint {
                    assert!(cold.checkpoint().unwrap());
                } else {
                    query(
                        &mut verifier,
                        &mut cold,
                        "REINDEX INDEX CONCURRENTLY receipt_reindex_rows_pkey_ccold",
                    );
                    assert_eq!(
                        first_row(&query(
                            &mut verifier,
                            &mut cold,
                            "SELECT r.rolname, x.indisvalid, x.indisready, x.indislive \
                         FROM pg_class i JOIN pg_index x ON x.indexrelid = i.oid \
                         JOIN pg_roles r ON r.oid = i.relowner \
                         WHERE i.relname = 'receipt_reindex_rows_pkey_ccold'"
                        )),
                        "receipt_reindex_owner|t|t|t"
                    );
                }
                cold.commit_wal().unwrap();
                drop((verifier, cold, cold_budget));
            }
        }
        drop(server);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }
}

#[test]
#[cfg(not(miri))]
fn concurrent_reindex_preserves_unbuilt_predecessor_flags_until_retirement() {
    use crate::sql::IndexCommitPhaseForTests as CommitPhase;
    let mut config = config("reindex-unbuilt-swap", crate::config::TransactionMode::Mvcc);
    config.object_store_on = true;
    config.object_store_bucket = format!("reindex-unbuilt-swap-{}", std::process::id());
    config.wal_upload = true;
    config.wal_upload_sync = true;
    config.wal_upload_buffer_bytes = 1 << 20;
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    let server = crate::sql::HttpObjectServer::start(&mut config);
    let mut budget = crate::sql::test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut builder = Conn::new(&config, &mut budget).unwrap();
    let mut observer = Conn::new(&config, &mut budget).unwrap();
    builder.id = 1;
    observer.id = 2;
    builder.enter_ready_for_tests(&engine);
    observer.enter_ready_for_tests(&engine);
    query(
        &mut builder,
        &mut engine,
        "CREATE TABLE unbuilt_swap_rows(id integer); INSERT INTO unbuilt_swap_rows VALUES(1),(1)",
    );
    let mut failed =
        b"CREATE UNIQUE INDEX CONCURRENTLY unbuilt_swap_idx ON unbuilt_swap_rows(id)".to_vec();
    failed.push(0);
    let response = message(&mut builder, &mut engine, wire::FMSG_QUERY, &failed, true);
    assert!(response.windows(5).any(|bytes| bytes == b"23505"));
    assert_eq!(
        first_row(&query(
            &mut observer,
            &mut engine,
            "SELECT i.indisvalid,i.indisready,i.indislive FROM pg_index i \
         JOIN pg_class c ON c.oid=i.indexrelid WHERE c.relname='unbuilt_swap_idx'"
        )),
        "f|f|t"
    );
    query(
        &mut builder,
        &mut engine,
        "TRUNCATE unbuilt_swap_rows; INSERT INTO unbuilt_swap_rows VALUES(1),(2)",
    );
    engine.commit_wal().unwrap();
    engine.enable_async_durability();
    bind_command(
        &mut builder,
        &mut engine,
        "REINDEX INDEX CONCURRENTLY unbuilt_swap_idx",
    );
    builder.send.clear();
    assert!(builder.recv.append(&frontend(wire::FMSG_EXECUTE, &[0; 5])));
    let mut inspected = false;
    let mut completed = false;
    let mut receipts = [None; 16];
    for receipt in &mut receipts {
        // Keep the private catalog state observable even on a fast HTTP reply.
        let responses = server.hold_upload_responses();
        let step = crate::mem::guard::forbid_alloc(|| builder.process_message(&mut engine));
        if matches!(step, Step::Continue) {
            assert!(
                !has_message(builder.send.readable(), wire::MSG_ERROR_RESPONSE),
                "{}",
                String::from_utf8_lossy(builder.send.readable())
            );
            completed = true;
            break;
        }
        assert!(
            matches!(step, Step::Parked),
            "{step:?}: {}",
            String::from_utf8_lossy(builder.send.readable())
        );
        let target = builder
            .txn
            .staged_commit()
            .expect("index phase awaits its receipt")
            .target;
        *receipt = Some(target);
        if matches!(
            target,
            crate::sql::txn::CommitTarget::ConcurrentIndex {
                phase: CommitPhase::ReindexDead,
                ..
            }
        ) {
            // The swap is committed, while retirement is still private.
            assert_eq!(
                first_row(&query(
                    &mut observer,
                    &mut engine,
                    "SELECT i.indisvalid,i.indisready,i.indislive FROM pg_index i \
                 JOIN pg_class c ON c.oid=i.indexrelid WHERE c.relname='unbuilt_swap_idx_ccold'"
                )),
                "f|f|t"
            );
            inspected = true;
        }
        drop(responses);
        engine.commit_wal().unwrap();
    }
    assert!(
        inspected && completed,
        "inspected={inspected}, completed={completed}, receipts={receipts:?}, transaction {:?}, response {}, fatal {:?}",
        builder.txn.mode,
        String::from_utf8_lossy(builder.send.readable()),
        engine.fatal_commit_failure()
    );
    assert_eq!(
        command_completion_in(builder.send.readable()),
        Some("REINDEX")
    );
    message(&mut builder, &mut engine, wire::FMSG_SYNC, &[], false);
    assert_eq!(
        first_row(&query(
            &mut observer,
            &mut engine,
            "SELECT i.indisvalid,i.indisready,i.indislive FROM pg_index i \
         JOIN pg_class c ON c.oid=i.indexrelid WHERE c.relname='unbuilt_swap_idx'"
        )),
        "t|t|t"
    );
    engine.commit_wal().unwrap();
    drop((builder, observer, engine, server));
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
}

#[test]
#[cfg(not(miri))]
fn concurrent_reindex_keeps_old_reader_lifetimes_until_their_locks_are_released() {
    use crate::sql::IndexCommitPhaseForTests as CommitPhase;
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let label = match mode {
            crate::config::TransactionMode::Mvcc => "mvcc",
            crate::config::TransactionMode::Locking => "locking",
        };
        let mut config = config(&format!("reindex-held-reader-{mode:?}"), mode);
        config.object_store_on = true;
        config.object_store_bucket = format!("reindex-reader-{}-{label}", std::process::id());
        config.wal_upload = true;
        config.wal_upload_sync = true;
        config.wal_upload_buffer_bytes = 1 << 20;
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let server = crate::sql::HttpObjectServer::start(&mut config);
        let mut budget = crate::sql::test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut builder = Conn::new(&config, &mut budget).unwrap();
        let mut reader = Conn::new(&config, &mut budget).unwrap();
        let mut observer = Conn::new(&config, &mut budget).unwrap();
        builder.id = 1;
        reader.id = 2;
        observer.id = 3;
        builder.enter_ready_for_tests(&engine);
        reader.enter_ready_for_tests(&engine);
        observer.enter_ready_for_tests(&engine);
        query(
            &mut builder,
            &mut engine,
            "CREATE TABLE held_reindex_rows(id integer); INSERT INTO held_reindex_rows VALUES(1),(2); \
             CREATE INDEX held_reindex_idx ON held_reindex_rows(id)",
        );
        let old = first_row(&query(
            &mut observer,
            &mut engine,
            "SELECT oid FROM pg_class WHERE relname='held_reindex_idx'",
        ));
        engine.commit_wal().unwrap();
        if mode == crate::config::TransactionMode::Locking {
            assert_eq!(
                first_row(&query(
                    &mut reader,
                    &mut engine,
                    "BEGIN; SELECT id FROM held_reindex_rows WHERE id=1"
                )),
                "1"
            );
        }
        engine.enable_async_durability();
        bind_command(
            &mut builder,
            &mut engine,
            "REINDEX INDEX CONCURRENTLY held_reindex_idx",
        );
        builder.send.clear();
        assert!(builder.recv.append(&frontend(wire::FMSG_EXECUTE, &[0; 5])));
        if mode == crate::config::TransactionMode::Mvcc {
            let mut waiting_to_swap = false;
            for _ in 0..8 {
                let step = crate::mem::guard::forbid_alloc(|| builder.process_message(&mut engine));
                assert!(
                    matches!(step, Step::Parked),
                    "{step:?}: {}",
                    String::from_utf8_lossy(builder.send.readable())
                );
                let target = builder
                    .txn
                    .staged_commit()
                    .expect("phase must await durable receipt")
                    .target;
                if matches!(
                    target,
                    crate::sql::txn::CommitTarget::ConcurrentIndex {
                        phase: CommitPhase::ReindexSwapped,
                        ..
                    }
                ) {
                    waiting_to_swap = true;
                    break;
                }
                engine.commit_wal().unwrap();
            }
            assert!(waiting_to_swap);
            // This reader enters after validation's old-snapshot check, but
            // before the replacement is visible. It genuinely reads the table
            // while the prior index is still a valid planning choice.
            assert_eq!(
                first_row(&query(
                    &mut reader,
                    &mut engine,
                    "BEGIN; SELECT id FROM held_reindex_rows WHERE id=1"
                )),
                "1"
            );
            engine.commit_wal().unwrap();
        }
        let step = crate::mem::guard::forbid_alloc(|| builder.process_message(&mut engine));
        assert!(
            matches!(step, Step::Parked),
            "{mode:?}: {step:?}: {}",
            String::from_utf8_lossy(builder.send.readable())
        );
        assert!(builder.txn.staged_commit().is_none());
        assert!(!has_message(
            builder.send.readable(),
            wire::MSG_COMMAND_COMPLETE
        ));
        if mode == crate::config::TransactionMode::Mvcc {
            assert_eq!(
                first_row(&query(
                    &mut observer,
                    &mut engine,
                    "SELECT i.indisvalid,i.indisready,i.indislive FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid \
                 WHERE c.relname='held_reindex_idx_ccold'"
                )),
                "f|t|t"
            );
        } else {
            assert_eq!(
                first_row(&query(
                    &mut observer,
                    &mut engine,
                    "SELECT count(*) FROM pg_class WHERE relname='held_reindex_idx_ccnew'"
                )),
                "0"
            );
            assert_eq!(
                first_row(&query(
                    &mut observer,
                    &mut engine,
                    "SELECT i.indisvalid,i.indisready,i.indislive FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid \
                 WHERE c.relname='held_reindex_idx'"
                )),
                "t|t|t"
            );
        }
        assert_eq!(
            first_row(&query(
                &mut reader,
                &mut engine,
                "SELECT count(*) FROM held_reindex_rows"
            )),
            "2"
        );
        query(&mut reader, &mut engine, "COMMIT");
        let mut completed = false;
        for _ in 0..12 {
            let step = crate::mem::guard::forbid_alloc(|| builder.process_message(&mut engine));
            if matches!(step, Step::Continue) {
                completed = true;
                break;
            }
            assert!(
                matches!(step, Step::Parked),
                "{mode:?}: {step:?}: {}",
                String::from_utf8_lossy(builder.send.readable())
            );
            assert!(
                builder.txn.staged_commit().is_some(),
                "reader released its actual table lock"
            );
            engine.commit_wal().unwrap();
        }
        assert!(completed);
        assert_eq!(
            command_completion_in(builder.send.readable()),
            Some("REINDEX")
        );
        message(&mut builder, &mut engine, wire::FMSG_SYNC, &[], false);
        assert_eq!(
            first_row(&query(
                &mut observer,
                &mut engine,
                &format!(
                    "SELECT count(*) FROM pg_class WHERE oid={old} OR relname IN ('held_reindex_idx_ccnew','held_reindex_idx_ccold')"
                )
            )),
            "0"
        );
        engine.commit_wal().unwrap();
        drop(builder);
        drop(reader);
        drop(observer);
        drop(engine);
        drop(server);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }
}

#[test]
#[cfg(not(miri))]
fn concurrent_index_receipts_keep_exact_effects_cancel_and_cold_states() {
    for with_triggers in [false, true] {
        for cancel_after_registration in [false, true] {
            let mut config = config(
                &format!("concurrent-index-receipts-{with_triggers}-{cancel_after_registration}"),
                crate::config::TransactionMode::Mvcc,
            );
            config.object_store_on = true;
            config.object_store_bucket = format!(
                "index-receipts-{}-{with_triggers}-{cancel_after_registration}",
                std::process::id()
            );
            config.wal_upload = true;
            config.wal_upload_sync = true;
            config.wal_upload_buffer_bytes = 1 << 20;
            crate::object_store::sim::drop_namespace(&config.object_store_bucket);
            let server = crate::sql::HttpObjectServer::start(&mut config);
            let mut budget = crate::sql::test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut builder = Conn::new(&config, &mut budget).unwrap();
            let mut observer = Conn::new(&config, &mut budget).unwrap();
            builder.id = 1;
            observer.id = 2;
            builder.enter_ready_for_tests(&engine);
            observer.enter_ready_for_tests(&engine);
            query(
                &mut builder,
                &mut engine,
                "CREATE TABLE receipt_index_rows(id integer); INSERT INTO receipt_index_rows VALUES(1),(2)",
            );
            if with_triggers {
                query(
                    &mut builder,
                    &mut engine,
                    "CREATE SEQUENCE receipt_index_start; CREATE SEQUENCE receipt_index_end; \
                     CREATE FUNCTION receipt_index_started() RETURNS event_trigger LANGUAGE plpgsql AS \
                     $$BEGIN PERFORM nextval('receipt_index_start'); END$$; \
                     CREATE FUNCTION receipt_index_finished() RETURNS event_trigger LANGUAGE plpgsql AS \
                     $$BEGIN PERFORM nextval('receipt_index_end'); END$$; \
                     CREATE EVENT TRIGGER receipt_index_start_trigger ON ddl_command_start \
                     WHEN TAG IN ('CREATE INDEX') EXECUTE FUNCTION receipt_index_started(); \
                     CREATE EVENT TRIGGER receipt_index_end_trigger ON ddl_command_end \
                     WHEN TAG IN ('CREATE INDEX') EXECUTE FUNCTION receipt_index_finished()",
                );
            }
            engine.commit_wal().unwrap();
            engine.enable_async_durability();
            bind_command(
                &mut builder,
                &mut engine,
                "CREATE INDEX CONCURRENTLY receipt_index_idx ON receipt_index_rows(id)",
            );
            builder.send.clear();
            assert!(builder.recv.append(&frontend(wire::FMSG_EXECUTE, &[0; 5])));
            let registration = server.hold_upload_responses();
            for _ in 0..3 {
                let step = crate::mem::guard::forbid_alloc(|| builder.process_message(&mut engine));
                assert!(
                    matches!(step, Step::Parked),
                    "{step:?}: {}",
                    String::from_utf8_lossy(builder.send.readable())
                );
                assert!(matches!(
                    builder.txn.staged_commit().unwrap().target,
                    crate::sql::txn::CommitTarget::ConcurrentIndex { .. }
                ));
                assert!(!has_message(
                    builder.send.readable(),
                    wire::MSG_COMMAND_COMPLETE
                ));
                assert!(!has_message(
                    builder.send.readable(),
                    wire::MSG_ERROR_RESPONSE
                ));
            }
            if cancel_after_registration {
                assert!(!crate::mem::guard::forbid_alloc(
                    || builder.cancel_parked(&mut engine)
                ));
                assert!(builder.txn.staged_commit().is_some());
            }
            drop(registration);
            engine.commit_wal().unwrap();
            let mut completed = false;
            for _ in 0..6 {
                let responses = server.hold_upload_responses();
                let step = crate::mem::guard::forbid_alloc(|| builder.process_message(&mut engine));
                if matches!(step, Step::Continue) {
                    completed = true;
                    break;
                }
                assert!(
                    matches!(step, Step::Parked),
                    "{step:?}: {}",
                    String::from_utf8_lossy(builder.send.readable())
                );
                assert!(matches!(
                    builder.txn.staged_commit().unwrap().target,
                    crate::sql::txn::CommitTarget::ConcurrentIndex { .. }
                ));
                assert!(!has_message(
                    builder.send.readable(),
                    wire::MSG_COMMAND_COMPLETE
                ));
                drop(responses);
                engine.commit_wal().unwrap();
            }
            assert!(
                completed,
                "bounded index phases did not complete: {}",
                String::from_utf8_lossy(builder.send.readable())
            );
            if cancel_after_registration {
                assert!(has_message(
                    builder.send.readable(),
                    wire::MSG_ERROR_RESPONSE
                ));
                assert!(
                    builder
                        .send
                        .readable()
                        .windows(5)
                        .any(|bytes| bytes == b"57014"),
                    "{}",
                    String::from_utf8_lossy(builder.send.readable())
                );
                assert!(!has_message(
                    builder.send.readable(),
                    wire::MSG_COMMAND_COMPLETE
                ));
                message(&mut builder, &mut engine, wire::FMSG_SYNC, &[], false);
            } else {
                assert!(
                    !has_message(builder.send.readable(), wire::MSG_ERROR_RESPONSE),
                    "{}",
                    String::from_utf8_lossy(builder.send.readable())
                );
                assert_eq!(
                    command_completion_in(builder.send.readable()),
                    Some("CREATE INDEX")
                );
                builder.send.clear();
                assert!(builder.recv.append(&frontend(wire::FMSG_SYNC, &[])));
                let completion = server.hold_upload_responses();
                let step = crate::mem::guard::forbid_alloc(|| builder.process_message(&mut engine));
                assert!(
                    matches!(step, Step::Parked),
                    "{step:?}: {}",
                    String::from_utf8_lossy(builder.send.readable())
                );
                assert!(builder.txn.staged_commit().is_some());
                let selected_table = engine
                    .publication_storage_for_tests()
                    .find_visible("public", "receipt_index_rows", 0)
                    .unwrap();
                let unavailable = crate::mem::guard::forbid_alloc(|| {
                    engine.publication_storage_for_tests().lock_table(
                        u32::MAX,
                        selected_table,
                        crate::sql::ast::TableLockMode::AccessExclusive,
                        true,
                    )
                });
                assert_eq!(
                    unavailable.unwrap_err().sqlstate,
                    sqlstate::LOCK_NOT_AVAILABLE
                );
                let mut held_by_final_transaction = false;
                engine.publication_storage_for_tests().visit_object_locks(
                    |owner, _, target, mode| {
                        held_by_final_transaction |= owner
                            == crate::storage::ObjectLockOwner::Transaction(builder.txn.txid)
                            && target == crate::storage::ObjectLockTarget::Table(selected_table)
                            && mode == crate::sql::ast::TableLockMode::ShareUpdateExclusive;
                    },
                );
                assert!(
                    held_by_final_transaction,
                    "final index transaction retains its selected table lock until publication"
                );
                observer.send.clear();
                assert!(observer.recv.append(&frontend(
                    wire::FMSG_QUERY,
                    b"DROP TABLE receipt_index_rows\0"
                )));
                let step =
                    crate::mem::guard::forbid_alloc(|| observer.process_message(&mut engine));
                assert!(
                    matches!(step, Step::Parked),
                    "final index transaction must protect its selected table: {step:?}: {}",
                    String::from_utf8_lossy(observer.send.readable())
                );
                assert!(crate::mem::guard::forbid_alloc(
                    || observer.cancel_parked(&mut engine)
                ));
                drop(completion);
                engine.commit_wal().unwrap();
                let step = crate::mem::guard::forbid_alloc(|| builder.process_message(&mut engine));
                assert!(
                    matches!(step, Step::Continue),
                    "{step:?}: {}",
                    String::from_utf8_lossy(builder.send.readable())
                );
            }
            assert_eq!(builder.txn.mode, crate::sql::txn::TxnMode::Idle);
            if with_triggers {
                assert_eq!(
                    first_row(&query(
                        &mut observer,
                        &mut engine,
                        "SELECT last_value,is_called FROM receipt_index_start"
                    )),
                    "1|t"
                );
                assert_eq!(
                    first_row(&query(
                        &mut observer,
                        &mut engine,
                        "SELECT last_value,is_called FROM receipt_index_end"
                    )),
                    if cancel_after_registration {
                        "1|f"
                    } else {
                        "1|t"
                    }
                );
            }
            engine.commit_wal().unwrap();
            drop(builder);
            drop(observer);
            drop(engine);
            std::fs::remove_dir_all(&config.data_dir).unwrap();
            let mut recovered_budget = crate::sql::test_engine_budget(&config, 1 << 29);
            let mut recovered = Engine::new(&config, &mut recovered_budget).unwrap();
            let mut recovered_conn = Conn::new(&config, &mut recovered_budget).unwrap();
            recovered_conn.id = 1;
            recovered_conn.enter_ready_for_tests(&recovered);
            assert_eq!(
                first_row(&query(
                    &mut recovered_conn,
                    &mut recovered,
                    "SELECT i.indisvalid,i.indisready,i.indislive FROM pg_index i \
                 JOIN pg_class c ON c.oid=i.indexrelid WHERE c.relname='receipt_index_idx'"
                )),
                if cancel_after_registration {
                    "f|f|t"
                } else {
                    "t|t|t"
                }
            );
            if cancel_after_registration {
                query(
                    &mut recovered_conn,
                    &mut recovered,
                    "REINDEX INDEX receipt_index_idx",
                );
                assert_eq!(
                    first_row(&query(
                        &mut recovered_conn,
                        &mut recovered,
                        "SELECT i.indisvalid,i.indisready,i.indislive FROM pg_index i \
                     JOIN pg_class c ON c.oid=i.indexrelid WHERE c.relname='receipt_index_idx'"
                    )),
                    "t|t|t"
                );
            }
            query(
                &mut recovered_conn,
                &mut recovered,
                "DROP INDEX CONCURRENTLY receipt_index_idx",
            );
            assert_eq!(
                first_row(&query(
                    &mut recovered_conn,
                    &mut recovered,
                    "SELECT count(*) FROM pg_class WHERE relname='receipt_index_idx'"
                )),
                "0"
            );
            drop(recovered_conn);
            drop(recovered);
            drop(server);
            crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        }
    }
}

#[test]
fn extended_write_type_descriptions_restore_the_owning_sessions_search_path() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for portal_description in [false, true] {
            let config = config(&format!("describe-session-path-{portal_description}"), mode);
            let mut budget = crate::sql::test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut writer = Conn::new(&config, &mut budget).unwrap();
            let mut observer = Conn::new(&config, &mut budget).unwrap();
            writer.id = 1;
            observer.id = 2;
            writer.enter_ready_for_tests(&engine);
            observer.enter_ready_for_tests(&engine);
            query(
                &mut writer,
                &mut engine,
                "CREATE SCHEMA description_home; CREATE SCHEMA description_other; CREATE TABLE description_home.orders(id integer); SET search_path=description_home,public",
            );
            let search_path = writer.guc.search_path();
            query(
                &mut observer,
                &mut engine,
                "SET search_path=description_other,public",
            );
            let database = crate::sql::execution_database_oid();
            engine.select_database(database).unwrap();
            parse(
                &mut writer,
                &mut engine,
                "INSERT INTO orders VALUES($1) RETURNING id",
                &[crate::sql::types::oid::INT4],
            );
            query(&mut observer, &mut engine, "SELECT current_schema");
            engine.select_database(database).unwrap();
            message(
                &mut writer,
                &mut engine,
                wire::FMSG_BIND,
                &bind_payload("result", &[Some(b"7")]),
                false,
            );
            assert_eq!(
                first_row(&query(&mut observer, &mut engine, "SELECT current_schema")),
                "description_other"
            );
            engine.select_database(database).unwrap();
            let description = message(
                &mut writer,
                &mut engine,
                wire::FMSG_DESCRIBE,
                if portal_description {
                    b"Presult\0"
                } else {
                    b"Sbound\0"
                },
                false,
            );
            assert!(has_message(&description, wire::MSG_ROW_DESCRIPTION));
            assert_eq!(
                has_message(&description, wire::MSG_PARAMETER_DESCRIPTION),
                !portal_description
            );
            assert_eq!(writer.guc.search_path(), search_path);
            assert_eq!(
                first_row(&query(&mut observer, &mut engine, "SELECT current_schema")),
                "description_other"
            );
            engine.select_database(database).unwrap();
            let mut execute = b"result\0".to_vec();
            execute.extend_from_slice(&0i32.to_be_bytes());
            assert_eq!(
                first_row(&message(
                    &mut writer,
                    &mut engine,
                    wire::FMSG_EXECUTE,
                    &execute,
                    false
                )),
                "7"
            );
            message(&mut writer, &mut engine, wire::FMSG_SYNC, &[], false);
            assert_eq!(
                first_row(&query(
                    &mut writer,
                    &mut engine,
                    "SELECT count(*) FROM orders"
                )),
                "1"
            );
        }
    }
}

fn parameter_description(mut bytes: &[u8]) -> Vec<i32> {
    while bytes.len() >= 5 {
        let total = 1 + i32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize;
        if bytes[0] == wire::MSG_PARAMETER_DESCRIPTION {
            let count = u16::from_be_bytes(bytes[5..7].try_into().unwrap()) as usize;
            return (0..count)
                .map(|index| {
                    i32::from_be_bytes(bytes[7 + index * 4..11 + index * 4].try_into().unwrap())
                })
                .collect();
        }
        bytes = &bytes[total..];
    }
    panic!("missing ParameterDescription")
}

/// PostgreSQL fixes parameter types at Parse: an undeterminable type fails
/// there with `42P18`, and Describe reports the types Parse chose even after
/// the schema that selected them changes.
#[test]
fn parse_fixes_parameter_types_and_reports_undeterminable_ones() {
    let mut config = config(
        "parse-parameter-types",
        crate::config::TransactionMode::Mvcc,
    );
    config.max_prepared = 3;
    let mut budget = crate::sql::test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut conn = Conn::new(&config, &mut budget).unwrap();
    conn.id = 1;
    conn.enter_ready_for_tests(&engine);
    query(
        &mut conn,
        &mut engine,
        "CREATE TYPE parse_mood AS ENUM ('sad', 'happy'); \
         CREATE TABLE parse_typed(id bigint)",
    );
    let mut payload = b"undetermined\0SELECT pg_typeof($1)\0".to_vec();
    payload.extend_from_slice(&0u16.to_be_bytes());
    let response = message(&mut conn, &mut engine, wire::FMSG_PARSE, &payload, true);
    let text = String::from_utf8_lossy(&response);
    assert!(text.contains("42P18"), "{text}");
    assert!(
        text.contains("could not determine data type of parameter $1"),
        "{text}"
    );
    message(&mut conn, &mut engine, wire::FMSG_SYNC, &[], false);
    parse(
        &mut conn,
        &mut engine,
        "SELECT $1::parse_mood, $2 FROM parse_typed WHERE id = $2",
        &[],
    );
    query(
        &mut conn,
        &mut engine,
        "ALTER TABLE parse_typed ALTER COLUMN id TYPE integer",
    );
    let response = message(
        &mut conn,
        &mut engine,
        wire::FMSG_DESCRIBE,
        b"Sbound\0",
        false,
    );
    let mood = first_row(&query(
        &mut conn,
        &mut engine,
        "SELECT 'parse_mood'::regtype::oid",
    ));
    assert_eq!(
        parameter_description(&response),
        [mood.parse::<i32>().unwrap(), crate::sql::types::oid::INT8]
    );
}
