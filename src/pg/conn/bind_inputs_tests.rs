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

fn bind_payload(parameters: &[&[u8]]) -> Vec<u8> {
    let mut payload = b"result\0bound\0".to_vec();
    payload.extend_from_slice(&0u16.to_be_bytes());
    payload.extend_from_slice(&(parameters.len() as u16).to_be_bytes());
    for parameter in parameters {
        payload.extend_from_slice(&(parameter.len() as i32).to_be_bytes());
        payload.extend_from_slice(parameter);
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
            &bind_payload(&[b"invalid"]),
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
                &bind_payload(&[b"9"]),
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
            let bind = frontend(wire::FMSG_BIND, &bind_payload(&[b"9"]));
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
            }
            if !cancel {
                message(&mut conn, &mut engine, wire::FMSG_SYNC, &[], false);
            }
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
        }
    }
}
