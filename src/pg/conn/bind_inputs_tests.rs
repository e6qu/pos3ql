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
    assert!(matches!(
        crate::mem::guard::forbid_alloc(|| conn.process_message(engine)),
        Step::Continue
    ));
    let response = conn.send.readable().to_vec();
    assert_eq!(
        has_message(&response, wire::MSG_ERROR_RESPONSE),
        error,
        "{}",
        String::from_utf8_lossy(&response)
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
                assert_eq!(
                    first_row(&query(
                        &mut conn,
                        &mut engine,
                        "SELECT value FROM bind_prepared WHERE id=1"
                    )),
                    "before"
                );
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
fn immediate_utility_resumes_durable_commit_without_repeating_ddl() {
    let mut config = config(
        "immediate-utility-wait",
        crate::config::TransactionMode::Mvcc,
    );
    config.object_store_on = true;
    config.object_store_sim = true;
    config.object_store_bucket = format!("immediate-utility-wait-{}", std::process::id());
    config.wal_upload = true;
    config.wal_upload_sync = true;
    config.wal_upload_buffer_bytes = 1 << 20;
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
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
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
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
                    expected_checks
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
