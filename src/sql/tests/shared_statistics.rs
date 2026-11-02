use super::*;

#[test]
fn analyzed_columns_merge_by_attribute_and_follow_transactional_definition_changes() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("shared-column-statistics-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(
            &mut engine,
            "CREATE TABLE statistics_merge(a integer,b integer); \
             INSERT INTO statistics_merge VALUES(1,10),(NULL,20); ANALYZE statistics_merge; \
             UPDATE statistics_merge SET b=NULL; ANALYZE statistics_merge(a)",
            true,
        );
        let rows = session.success(&mut engine,
            "SELECT attname,null_frac FROM pg_stats WHERE tablename='statistics_merge' ORDER BY attname", true);
        assert_eq!(data_rows(&rows), ["a|0.5", "b|0"]);
        session.success(&mut engine, "ANALYZE statistics_merge(b)", true);
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT attname,null_frac FROM pg_stats WHERE tablename='statistics_merge' ORDER BY attname", true)),
            ["a|0.5", "b|1"]);

        session.success(&mut engine, "BEGIN; SAVEPOINT original_statistics", true);
        for alteration in [
            "ALTER TABLE statistics_merge ALTER COLUMN a TYPE bigint",
            "ALTER TABLE statistics_merge ALTER COLUMN a TYPE integer",
            "ALTER TABLE statistics_merge ALTER COLUMN a TYPE integer USING a+1",
        ] {
            session.success(&mut engine, alteration, true);
            assert!(session.transaction.statistics_undo().is_empty());
            assert_eq!(data_rows(&session.success(&mut engine,
                "SELECT attname,null_frac FROM pg_stats WHERE tablename='statistics_merge' ORDER BY attname", true)),
                ["b|1"]);
            session.success(&mut engine, "ROLLBACK TO original_statistics", true);
            assert_eq!(data_rows(&session.success(&mut engine,
                "SELECT attname,null_frac FROM pg_stats WHERE tablename='statistics_merge' ORDER BY attname", true)),
                ["a|0.5", "b|1"]);
        }
        session.success(
            &mut engine,
            "ALTER TABLE statistics_merge DROP COLUMN b",
            true,
        );
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT attname,null_frac FROM pg_stats WHERE tablename='statistics_merge' ORDER BY attname", true)),
            ["a|0.5"]);
        session.success(&mut engine, "ROLLBACK TO original_statistics; COMMIT", true);
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT attname,null_frac FROM pg_stats WHERE tablename='statistics_merge' ORDER BY attname", true)),
            ["a|0.5", "b|1"]);

        // Reusing a spelling does not give the new attribute its predecessor's data.
        session.success(&mut engine,
            "ALTER TABLE statistics_merge DROP COLUMN b; ALTER TABLE statistics_merge ADD COLUMN b integer", true);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT attname FROM pg_stats WHERE tablename='statistics_merge' ORDER BY attname",
                true
            )),
            ["a"]
        );
        session.success(&mut engine, "ANALYZE statistics_merge(b)", true);
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT attname,null_frac FROM pg_stats WHERE tablename='statistics_merge' ORDER BY attname", true)),
            ["a|0.5", "b|1"]);
    }
}

#[test]
fn prepared_definition_statistics_resolve_cold_with_the_shared_pool_full() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for checkpoint in [false, true] {
            for commit in [false, true] {
                let mode_name = match mode {
                    crate::config::TransactionMode::Mvcc => "mvcc",
                    crate::config::TransactionMode::Locking => "locking",
                };
                let name = format!("prepared-statistics-{mode_name}-{checkpoint}-{commit}");
                let mut config = test_config(&name);
                config.transaction_mode = mode;
                config.statistics_bytes = 128;
                config.statistics_leases = 2;
                config.max_analyze_per_transaction = 1;
                config.max_prepared_transactions = 1;
                config.max_ddl_per_transaction = 8;
                config.max_tables = 2;
                config.row_overlay_slots = 8;
                config.txn_rows = 8;
                config.object_store_on = true;
                config.object_store_sim = true;
                config.wal_upload = true;
                config.wal_upload_sync = true;
                config.object_store_bucket = format!("{name}-{}", std::process::id());
                crate::object_store::sim::drop_namespace(&config.object_store_bucket);
                let server = object_reads::HttpObjectServer::start(&mut config);
                let mut budget = test_engine_budget(&config, 1 << 29);
                let mut engine = Engine::new(&config, &mut budget).unwrap();
                let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
                session.success(&mut engine,
                    "CREATE TABLE prepared_statistics(a integer,b integer); CREATE TABLE statistics_quota(); \
                     INSERT INTO prepared_statistics VALUES(1,10),(2,20); ANALYZE prepared_statistics; \
                     BEGIN; ANALYZE statistics_quota; \
                     ALTER TABLE prepared_statistics ALTER COLUMN a TYPE integer USING a+1", true);
                assert_eq!(session.transaction.statistics_undo().len(), 1);
                assert_eq!(data_rows(&session.success(&mut engine,
                    "SELECT attname FROM pg_stats WHERE tablename='prepared_statistics' ORDER BY attname", true)), ["b"]);
                session.success(&mut engine, "PREPARE TRANSACTION 'column-statistics'", true);
                assert_statistics_pool_full(&engine);
                if checkpoint {
                    assert!(engine.checkpoint().unwrap());
                }
                engine.commit_wal().unwrap();
                drop((session, engine));
                std::fs::remove_dir_all(&config.data_dir).unwrap();

                let mut cold_budget = test_engine_budget(&config, 1 << 29);
                let mut recovered = Engine::new(&config, &mut cold_budget).unwrap();
                assert_statistics_pool_full(&recovered);
                let mut resolver = ConfiguredTransactionSession::new(&config, &mut cold_budget);
                resolver.success(
                    &mut recovered,
                    if commit {
                        "COMMIT PREPARED 'column-statistics'"
                    } else {
                        "ROLLBACK PREPARED 'column-statistics'"
                    },
                    true,
                );
                let rows = data_rows(&resolver.success(&mut recovered,
                    "SELECT attname FROM pg_stats WHERE tablename='prepared_statistics' ORDER BY attname; \
                     SELECT a,b FROM prepared_statistics ORDER BY a", true));
                assert_eq!(
                    rows,
                    if commit {
                        vec!["b", "2|10", "3|20"]
                    } else {
                        vec!["a", "b", "1|10", "2|20"]
                    }
                );
                // Resolution released the discarded root without needing temporary capacity.
                crate::mem::guard::forbid_alloc(|| {
                    let spare = recovered.storage.statistics_pool().lease::<u8>(1).unwrap();
                    drop(spare);
                });
                assert!(recovered.checkpoint().unwrap());
                drop((resolver, recovered));
                std::fs::remove_dir_all(&config.data_dir).unwrap();
                drop(server);
                crate::object_store::sim::drop_namespace(&config.object_store_bucket);
            }
        }
    }
}

fn assert_statistics_pool_full(engine: &Engine) {
    crate::mem::guard::forbid_alloc(|| {
        let Err(error) = engine.storage.statistics_pool().lease::<u8>(1) else {
            panic!("committed and prepared statistics must occupy both startup leases");
        };
        assert_eq!(error.free_bytes, 0);
        assert_eq!(error.available_leases, 0);
    });
}
