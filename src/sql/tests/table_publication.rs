use super::*;

#[test]
fn table_publication_sql_renames_indexes_comments_sequences_and_cold_recovery() {
    for checkpoint in [false, true] {
        for mode in [crate::config::TransactionMode::Mvcc, crate::config::TransactionMode::Locking] {
            let name = format!("table-publication-sql-{checkpoint}-{mode:?}");
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
            assert_eq!(data_rows(&session.success(&mut engine,
                include_str!("../../../tests/external/differential_exact/20_table_publication.sql"), false)),
                ["1|10", "1", "3", "published table", "published index", "1", "2|30", "2"]);
            if checkpoint { assert!(engine.checkpoint().unwrap()); }
            drop(session);
            drop(engine);
            std::fs::remove_dir_all(&config.data_dir).unwrap();
            let mut cold_budget = test_engine_budget(&config, 1 << 29);
            let mut cold = Engine::new(&config, &mut cold_budget).unwrap();
            let mut session = ConfiguredTransactionSession::new(&config, &mut cold_budget);
            assert_eq!(data_rows(&session.success(&mut cold,
                "SELECT count(*), sum(amount) FROM publication_target_sql.publication_renamed_sql; \
                 SELECT count(*) FROM publication_child_sql; \
                 SELECT obj_description('publication_target_sql.publication_key_sql'::regclass)", false)),
                ["2|30", "2", "published index"]);
            assert_eq!(error_sqlstates(&session.execute(&mut cold,
                "INSERT INTO publication_child_sql VALUES (99)", false)), ["23503"]);
            session.success(&mut cold,
                "INSERT INTO publication_target_sql.publication_renamed_sql VALUES (3, 30); \
                 INSERT INTO publication_child_sql VALUES (3)", false);
            assert_eq!(data_rows(&session.success(&mut cold,
                "SELECT id, amount FROM publication_target_sql.publication_renamed_sql WHERE amount = 30", false)), ["3|30"]);
            drop(session);
            drop(cold);
            crate::object_store::sim::drop_namespace(&config.object_store_bucket);
            std::fs::remove_dir_all(&config.data_dir).unwrap();
        }
    }
}
