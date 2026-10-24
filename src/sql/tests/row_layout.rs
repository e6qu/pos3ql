use super::*;

#[test]
fn row_layout_sql_rewrites_rollback_temporary_reuse_and_cold_recovery() {
    for checkpoint in [false, true] {
        for mode in [
            crate::config::TransactionMode::Mvcc,
            crate::config::TransactionMode::Locking,
        ] {
            let name = format!("row-layout-sql-{checkpoint}-{mode:?}");
            let mut config = test_config(&name);
            config.transaction_mode = mode;
            config.table_rows = 512;
            config.txn_rows = 512;
            config.object_store_on = true;
            config.object_store_sim = true;
            config.wal_upload = true;
            config.wal_upload_sync = true;
            config.object_store_bucket = format!("{name}-{}", std::process::id());
            crate::object_store::sim::drop_namespace(&config.object_store_bucket);
            let mut budget = test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
            assert_eq!(
                data_rows(&session.success(
                    &mut engine,
                    include_str!("../../../tests/external/differential_exact/18_row_layout.sql"),
                    false
                )),
                [
                    "130|8515|integer",
                    "11|order|bigint",
                    "8|bigint",
                    "replacement"
                ]
            );
            if checkpoint {
                assert!(engine.checkpoint().unwrap());
            }
            drop(session);
            drop(engine);
            std::fs::remove_dir_all(&config.data_dir).unwrap();
            let mut cold_budget = test_engine_budget(&config, 1 << 29);
            let mut cold = Engine::new(&config, &mut cold_budget).unwrap();
            let mut session = ConfiguredTransactionSession::new(&config, &mut cold_budget);
            assert_eq!(data_rows(&session.success(&mut cold, "SELECT id, note, pg_typeof(id)::text FROM layout_orders; SELECT count(*) FROM pg_class WHERE relname = 'layout_temporary'", false)), ["11|order|bigint", "0"]);
            assert_eq!(
                error_sqlstates(&session.execute(
                    &mut cold,
                    "INSERT INTO layout_orders VALUES (11, 'duplicate')",
                    false
                )),
                ["23505"]
            );
            session.success(&mut cold, "UPDATE layout_orders SET id = 12; DELETE FROM layout_orders WHERE id = 12; INSERT INTO layout_orders VALUES (13, 'fresh')", false);
            assert_eq!(
                data_rows(&session.success(&mut cold, "SELECT id, note FROM layout_orders", false)),
                ["13|fresh"]
            );
        }
    }
}

#[test]
fn row_layout_retained_history_survives_saved_publication_and_prepared_rewrite() {
    for checkpoint in [false, true] {
        let name = format!("row-layout-history-{checkpoint}");
        let mut config = test_config(&name);
        config.transaction_mode = crate::config::TransactionMode::Mvcc;
        config.table_rows = 512;
        config.txn_rows = 512;
        config.max_prepared_transactions = 1;
        config.object_store_on = true;
        config.object_store_sim = true;
        config.wal_upload = true;
        config.wal_upload_sync = true;
        config.object_store_bucket = format!("{name}-{}", std::process::id());
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
        let mut reader =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(&mut engine, "CREATE TABLE layout_orders(id integer PRIMARY KEY); CREATE TABLE layout_invoices(id integer); INSERT INTO layout_orders SELECT id FROM generate_series(1, 130) AS rows(id); INSERT INTO layout_invoices VALUES (22)", false);
        if checkpoint {
            assert!(engine.checkpoint().unwrap());
        }
        reader.success(
            &mut engine,
            "BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT id FROM layout_invoices",
            false,
        );
        writer.success(&mut engine, "DELETE FROM layout_orders WHERE id > 1", false);
        writer.success(&mut engine, "BEGIN; ALTER TABLE layout_orders ALTER COLUMN id TYPE bigint; PREPARE TRANSACTION 'layout_rewrite'", false);
        assert_eq!(
            error_sqlstates(&writer.execute(
                &mut engine,
                "SELECT 1; COMMIT PREPARED 'layout_rewrite'",
                false
            )),
            ["25001"]
        );
        writer.success(&mut engine, "COMMIT PREPARED 'layout_rewrite'", false);
        assert_eq!(
            data_rows(&reader.success(
                &mut engine,
                "SELECT count(*) FROM layout_orders; SELECT id FROM layout_invoices",
                false
            )),
            ["0", "22"]
        );
        assert_eq!(
            data_rows(&writer.success(
                &mut engine,
                "SELECT id, pg_typeof(id)::text FROM layout_orders",
                false
            )),
            ["1|bigint"]
        );
        assert!(engine.checkpoint().unwrap());
        writer.success(&mut engine, "UPDATE layout_orders SET id = 2; DELETE FROM layout_orders WHERE id = 2; INSERT INTO layout_orders VALUES (3)", false);
        assert_eq!(
            data_rows(&writer.success(&mut engine, "SELECT id FROM layout_orders", false)),
            ["3"]
        );
        reader.success(&mut engine, "COMMIT", false);
        drop(reader);
        drop(writer);
        drop(engine);
        std::fs::remove_dir_all(&config.data_dir).unwrap();
        let mut cold_budget = test_engine_budget(&config, 1 << 29);
        let mut cold = Engine::new(&config, &mut cold_budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut cold_budget);
        assert_eq!(
            data_rows(&session.success(
                &mut cold,
                "SELECT id, pg_typeof(id)::text FROM layout_orders; SELECT id FROM layout_invoices",
                false
            )),
            ["3|bigint", "22"]
        );
    }
}
