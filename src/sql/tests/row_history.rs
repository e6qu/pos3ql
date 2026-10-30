use super::*;

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
            true,
        );
        assert_eq!(data_rows(&reader.success(&mut engine,
            "BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT id,value FROM history_orders ORDER BY id", true)),
            ["1|0","2|0"]);
        writer.success(
            &mut engine,
            "UPDATE history_orders SET value=1 WHERE id=2",
            true,
        );
        writer.success(&mut engine, "BEGIN", true);
        let durable_before = engine.replication_identity().1;
        let error = writer.execute(&mut engine, "UPDATE history_orders SET value=value+1", true);
        assert_eq!(error_sqlstates(&error), ["54000"]);
        assert!(String::from_utf8_lossy(&error).contains("max_row_versions_per_row"));
        writer.success(&mut engine, "COMMIT", true);
        assert_eq!(
            engine.replication_identity().1,
            durable_before,
            "a rejected multirow commit must not become durable"
        );
        assert_eq!(
            data_rows(&writer.success(
                &mut engine,
                "SELECT id,value FROM history_orders ORDER BY id",
                true
            )),
            ["1|0", "2|1"]
        );
        assert_eq!(
            data_rows(&reader.success(
                &mut engine,
                "SELECT id,value FROM history_orders ORDER BY id; COMMIT",
                true
            )),
            ["1|0", "2|0"]
        );
        writer.success(
            &mut engine,
            "UPDATE history_orders SET value=value+10",
            true,
        );
        assert_eq!(
            data_rows(&writer.success(
                &mut engine,
                "SELECT id,value FROM history_orders ORDER BY id",
                true
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
