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
        conn.phase = Phase::Ready;
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
        conn.phase = Phase::Ready;
        observer.phase = Phase::Ready;
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
    conn.phase = Phase::Ready;
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
    recovered_conn.phase = Phase::Ready;
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
        conn.phase = Phase::Ready;
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
        conn.phase = Phase::Ready;
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
        conn.phase = Phase::Ready;
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
        conn.phase = Phase::Ready;
        observer.phase = Phase::Ready;
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
                connection.phase = Phase::Ready;
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
        conn.phase = Phase::Ready;
        observer.phase = Phase::Ready;
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
        conn.phase = Phase::Ready;
        observer.phase = Phase::Ready;
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
        builder.phase = Phase::Ready;
        observer.phase = Phase::Ready;
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
        builder.phase = Phase::Ready;
        observer.phase = Phase::Ready;
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
fn concurrent_reindex_cancel_keeps_the_published_replacement_and_old_constraint() {
    let config = config(
        "concurrent-reindex-cancel",
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
    builder.phase = Phase::Ready;
    writer.phase = Phase::Ready;
    observer.phase = Phase::Ready;
    query(
        &mut builder,
        &mut engine,
        "CREATE TABLE reindex_cancel_rows(id integer PRIMARY KEY DEFERRABLE INITIALLY DEFERRED); INSERT INTO reindex_cancel_rows VALUES(1)",
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
    assert_eq!(
        first_row(&query(
            &mut observer,
            &mut engine,
            "SELECT i.indisvalid,i.indisready,i.indislive,i.indimmediate FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid \
         WHERE c.relname='reindex_cancel_rows_pkey_ccnew'"
        )),
        "f|f|t|f"
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
    builder.phase = Phase::Ready;
    writer.phase = Phase::Ready;
    observer.phase = Phase::Ready;
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
    builder.phase = Phase::Ready;
    writer.phase = Phase::Ready;
    observer.phase = Phase::Ready;
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
            builder.phase = Phase::Ready;
            observer.phase = Phase::Ready;
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
            engine.commit_wal().unwrap();
            let mut completed = false;
            for _ in 0..6 {
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
            recovered_conn.phase = Phase::Ready;
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
