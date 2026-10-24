use super::*;

#[test]
fn key_preparation_sql_expression_failure_rollback_reindex_and_cold_recovery() {
    for checkpoint in [false, true] {
        for mode in [
            crate::config::TransactionMode::Mvcc,
            crate::config::TransactionMode::Locking,
        ] {
            let name = format!("key-preparation-sql-{checkpoint}-{mode:?}");
            let mut config = test_config(&name);
            config.transaction_mode = mode;
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
                    include_str!(
                        "../../../tests/external/differential_exact/19_key_preparation.sql"
                    ),
                    false
                )),
                ["1", "1|11", "3|20|23", "2|5|42"]
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
            assert_eq!(data_rows(&session.success(&mut cold, "SELECT id, amount FROM key_orders_sql WHERE id + amount = 23; SELECT count(*) FROM pg_indexes WHERE tablename = 'key_orders_sql'", false)), ["3|20", "2"]);
            assert_eq!(
                error_sqlstates(&session.execute(
                    &mut cold,
                    "INSERT INTO key_orders_sql VALUES (3, 30)",
                    false
                )),
                ["23505"]
            );
            session.success(&mut cold, "UPDATE key_orders_sql SET amount = 30 WHERE id = 3; DELETE FROM key_orders_sql WHERE id = 2", false);
            assert_eq!(
                data_rows(&session.success(
                    &mut cold,
                    "SELECT id, amount FROM key_orders_sql WHERE id + amount = 33",
                    false
                )),
                ["3|30"]
            );
            drop(session);
            drop(cold);
            crate::object_store::sim::drop_namespace(&config.object_store_bucket);
            std::fs::remove_dir_all(&config.data_dir).unwrap();
        }
    }
}
