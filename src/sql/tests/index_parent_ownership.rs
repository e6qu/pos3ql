use super::*;

#[test]
fn parent_owner_changes_reach_all_indexes_and_survive_rollback_and_cold_recovery() {
    for (mode, label) in [
        (crate::config::TransactionMode::Mvcc, "mvcc"),
        (crate::config::TransactionMode::Locking, "locking"),
    ] {
        let mut config = test_config(&format!("index-parent-owner-{label}"));
        config.transaction_mode = mode;
        config.object_store_on = true;
        config.object_store_bucket = format!("index-parent-owner-{label}-{}", std::process::id());
        config.wal_upload = true;
        config.wal_upload_sync = true;
        config.block_cache_bytes = 0;
        config.disk_cache_bytes = 0;
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let server = object_reads::HttpObjectServer::start(&mut config);
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let fixture =
            include_str!("../../../tests/external/differential/226_index_parent_ownership.sql");
        let (setup, cleanup) = fixture.split_once("DROP MATERIALIZED VIEW").unwrap();
        let mut rows = Vec::new();
        for statement in setup
            .split(";\n")
            .map(str::trim)
            .filter(|sql| !sql.is_empty())
        {
            rows.extend(data_rows(&session.success(&mut engine, statement, true)));
        }
        assert_eq!(
            rows,
            [
                "initial|t|t",
                "pending|t|t",
                "rolled-back|t|t",
                "committed|t|t",
                "view-committed|t|t",
                "view-pending|t|t",
                "view-rolled-back|t|t",
            ],
            "{label}"
        );
        engine.commit_wal().unwrap();
        assert!(server.uploads.load(std::sync::atomic::Ordering::Acquire) > 0);
        drop((session, engine, budget));
        for checkpoint in [false, true] {
            std::fs::remove_dir_all(&config.data_dir).unwrap();
            let mut cold_budget = test_engine_budget(&config, 1 << 29);
            let mut recovered = Engine::new(&config, &mut cold_budget).unwrap();
            let mut verifier = ConfiguredTransactionSession::new(&config, &mut cold_budget);
            for relation in ["index_parent_rows", "index_parent_view"] {
                let sql = format!(
                    "SELECT count(*) > 0, bool_and(t.relowner = r.oid AND i.relowner = r.oid) \
                     FROM pg_class t JOIN pg_index x ON x.indrelid = t.oid OR x.indrelid = t.reltoastrelid \
                     JOIN pg_class i ON i.oid = x.indexrelid \
                     JOIN pg_roles r ON r.rolname = 'index_parent_owner' \
                     WHERE t.oid = '{relation}'::regclass"
                );
                assert_eq!(
                    data_rows(&verifier.success(&mut recovered, &sql, true)),
                    ["t|t"],
                    "{label}: {relation}, checkpoint={checkpoint}"
                );
            }
            if checkpoint {
                let sql = format!("DROP MATERIALIZED VIEW{cleanup}");
                for statement in sql
                    .split(";\n")
                    .map(str::trim)
                    .filter(|sql| !sql.is_empty())
                {
                    verifier.success(&mut recovered, statement, true);
                }
            } else {
                assert!(recovered.checkpoint().unwrap());
            }
            recovered.commit_wal().unwrap();
            drop((verifier, recovered, cold_budget));
        }
        drop(server);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }
}
