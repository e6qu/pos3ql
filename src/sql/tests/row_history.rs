use super::*;

#[test]
fn row_history_exhaustion_is_shared_and_owned_before_a_later_reader_starts() {
    let mut config = test_config("shared-history-admission");
    config.row_history_slots = 1;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut first = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(11);
    let mut second = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(22);
    let mut reader = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(33);
    first.success(
        &mut engine,
        "CREATE TABLE history_first(id integer PRIMARY KEY,value integer); \
        CREATE TABLE history_second(id integer PRIMARY KEY,value integer); \
        INSERT INTO history_first VALUES(1,0); INSERT INTO history_second VALUES(1,0)",
        true,
    );
    first.success(&mut engine, "BEGIN; UPDATE history_first SET value=1", true);
    let durable_before = engine.replication_identity().1;
    let rejected = second.execute(&mut engine, "UPDATE history_second SET value=2", true);
    assert_eq!(error_sqlstates(&rejected), ["54000"]);
    assert!(String::from_utf8_lossy(&rejected).contains("row_history_slots"));
    assert_eq!(engine.replication_identity().1, durable_before);
    assert_eq!(
        data_rows(&reader.success(
            &mut engine,
            "BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT value FROM history_first",
            true
        )),
        ["0"]
    );
    first.success(&mut engine, "COMMIT", true);
    assert_eq!(
        data_rows(&reader.success(
            &mut engine,
            "SELECT value FROM history_first; SELECT value FROM history_second; COMMIT",
            true
        )),
        ["0", "0"]
    );
    first.success(
        &mut engine,
        "BEGIN; UPDATE history_first SET value=3; SAVEPOINT later; \
        UPDATE history_first SET value=4; ROLLBACK TO later; ROLLBACK",
        true,
    );
    second.success(&mut engine, "UPDATE history_second SET value=5", true);
    assert_eq!(
        data_rows(&second.success(
            &mut engine,
            "SELECT value FROM history_first; SELECT value FROM history_second",
            true
        )),
        ["1", "5"]
    );
}

#[test]
fn row_history_reservation_survives_prepared_recovery_and_releases_on_resolution() {
    for cold in [false, true] {
        for commit in [false, true] {
            let mut config = test_config(&format!("prepared-history-{cold}-{commit}"));
            config.max_prepared_transactions = 1;
            config.row_history_slots = 2;
            let mut budget = test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut writer =
                ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(11);
            writer.success(
                &mut engine,
                "CREATE SEQUENCE history_reclaim_calls START 2; \
                CREATE TABLE prepared_history(id integer PRIMARY KEY,value integer); \
                INSERT INTO prepared_history VALUES(1,0),(2,0),(3,0); \
                BEGIN; UPDATE prepared_history SET value=1 WHERE id=1; \
                DELETE FROM prepared_history WHERE id=2; PREPARE TRANSACTION 'history-slot'",
                true,
            );
            drop(writer);
            if cold {
                drop(engine);
                budget = test_engine_budget(&config, 1 << 29);
                engine = Engine::new(&config, &mut budget).unwrap();
            }
            let mut resolver =
                ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(22);
            let error = resolver.execute(
                &mut engine,
                "UPDATE prepared_history SET value=9 WHERE id=3",
                true,
            );
            assert_eq!(error_sqlstates(&error), ["54000"]);
            assert!(String::from_utf8_lossy(&error).contains("row_history_slots"));
            let mut reader =
                ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(33);
            assert_eq!(data_rows(&reader.success(&mut engine,
                "BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT value FROM prepared_history ORDER BY id",true)),["0","0","0"]);
            resolver.success(
                &mut engine,
                if commit {
                    "COMMIT PREPARED 'history-slot'"
                } else {
                    "ROLLBACK PREPARED 'history-slot'"
                },
                true,
            );
            assert_eq!(
                data_rows(&reader.success(
                    &mut engine,
                    "SELECT value FROM prepared_history ORDER BY id; COMMIT",
                    true
                )),
                ["0", "0", "0"]
            );
            resolver.success(
                &mut engine,
                "UPDATE prepared_history SET value=value WHERE id=1; \
                UPDATE prepared_history SET value=nextval('history_reclaim_calls') WHERE id=3",
                true,
            );
            let rows = data_rows(&resolver.success(
                &mut engine,
                "SELECT value FROM prepared_history ORDER BY id",
                true,
            ));
            assert_eq!(
                rows,
                if commit {
                    vec!["1", "2"]
                } else {
                    vec!["0", "0", "2"]
                }
            );
            assert_eq!(
                data_rows(&resolver.success(
                    &mut engine,
                    "SELECT last_value,is_called FROM history_reclaim_calls",
                    true
                )),
                ["2|t"]
            );
        }
    }
}

#[test]
fn row_history_exhaustion_rejects_before_durable_publication_and_recovers() {
    for remote in [false, true] {
        let name = if remote {
            "history-boundary-objects"
        } else {
            "history-boundary-local"
        };
        let mut config = test_config(name);
        config.max_row_versions_per_row = 1;
        config.object_store_on = remote;
        config.object_store_sim = remote;
        config.wal_upload = remote;
        config.wal_upload_sync = remote;
        config.object_store_bucket = format!("{name}-{}", std::process::id());
        if remote {
            crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        }
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut reader =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(11);
        let mut writer =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(22);
        writer.success(
            &mut engine,
            "CREATE TABLE history_orders(id integer PRIMARY KEY,value integer); \
            INSERT INTO history_orders VALUES(1,0),(2,0)",
            !remote,
        );
        assert_eq!(data_rows(&reader.success(&mut engine,
            "BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT id,value FROM history_orders ORDER BY id", !remote)),
            ["1|0","2|0"]);
        writer.success(
            &mut engine,
            "UPDATE history_orders SET value=1 WHERE id=2",
            !remote,
        );
        writer.success(&mut engine, "BEGIN", !remote);
        let durable_before = engine.replication_identity().1;
        let error = writer.execute(
            &mut engine,
            "UPDATE history_orders SET value=value+1",
            !remote,
        );
        assert_eq!(error_sqlstates(&error), ["54000"]);
        assert!(String::from_utf8_lossy(&error).contains("max_row_versions_per_row"));
        writer.success(&mut engine, "COMMIT", !remote);
        assert_eq!(
            engine.replication_identity().1,
            durable_before,
            "a rejected multirow commit must not become durable"
        );
        assert_eq!(
            data_rows(&writer.success(
                &mut engine,
                "SELECT id,value FROM history_orders ORDER BY id",
                !remote
            )),
            ["1|0", "2|1"]
        );
        assert_eq!(
            data_rows(&reader.success(
                &mut engine,
                "SELECT id,value FROM history_orders ORDER BY id; COMMIT",
                !remote
            )),
            ["1|0", "2|0"]
        );
        writer.success(
            &mut engine,
            "UPDATE history_orders SET value=value+10",
            !remote,
        );
        assert_eq!(
            data_rows(&writer.success(
                &mut engine,
                "SELECT id,value FROM history_orders ORDER BY id",
                !remote
            )),
            ["1|10", "2|11"]
        );
        drop(reader);
        drop(writer);
        drop(engine);
        if remote {
            std::fs::remove_dir_all(&config.data_dir).unwrap();
        }
        let mut cold_budget = test_engine_budget(&config, 1 << 29);
        let mut recovered = Engine::new(&config, &mut cold_budget).unwrap();
        assert_eq!(
            data_rows(&run_with(
                &mut recovered,
                &mut cold_budget,
                "SELECT id,value FROM history_orders ORDER BY id"
            )),
            ["1|10", "2|11"]
        );
        drop(recovered);
        if remote {
            crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        }
        std::fs::remove_dir_all(&config.data_dir).unwrap();
    }
}
