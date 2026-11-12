use super::*;

const SETUP: &[&str] = &[
    "CREATE TABLE pip(a int, b int) PARTITION BY RANGE (a)",
    "CREATE UNIQUE INDEX pip_a ON pip (a, b)",
    "CREATE TABLE pip1 PARTITION OF pip FOR VALUES FROM (0) TO (10)",
    "CREATE TABLE pip2 (b int, a int)",
    "ALTER TABLE pip ATTACH PARTITION pip2 FOR VALUES FROM (10) TO (20)",
    "CREATE TABLE pip3 PARTITION OF pip FOR VALUES FROM (20) TO (30) PARTITION BY LIST (b)",
    "CREATE TABLE pip31 PARTITION OF pip3 FOR VALUES IN (1)",
    "INSERT INTO pip VALUES (1, 1), (11, 1), (21, 1)",
];

const INDEXES: &str = "SELECT c.relname, coalesce(p.relname, '-') FROM pg_index i \
     JOIN pg_class c ON c.oid = i.indexrelid \
     LEFT JOIN pg_inherits h ON h.inhrelid = i.indexrelid \
     LEFT JOIN pg_class p ON p.oid = h.inhparent \
     WHERE c.relname LIKE 'pip%' ORDER BY 1";

const EXPECTED: &[&str] = &[
    "pip1_a_b_idx|pip_a",
    "pip2_a_b_idx|pip_a",
    "pip31_a_b_idx|pip3_a_b_idx",
    "pip3_a_b_idx|pip_a",
    "pip_a|-",
];

fn recovery_config(label: &str, mode: crate::config::TransactionMode) -> Config {
    let mut config = test_config(&format!("partition-index-{label}"));
    config.transaction_mode = mode;
    config.object_store_on = true;
    config.object_store_bucket = format!("partition-index-{label}-{}", std::process::id());
    config.wal_upload = true;
    config.wal_upload_sync = true;
    config.block_cache_bytes = 0;
    config.disk_cache_bytes = 0;
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    config
}

#[test]
fn partitions_created_or_attached_later_enforce_the_parent_unique_index_after_cold_recovery() {
    for (mode, label) in [
        (crate::config::TransactionMode::Mvcc, "mvcc"),
        (crate::config::TransactionMode::Locking, "locking"),
    ] {
        let mut config = recovery_config(label, mode);
        let server = object_reads::HttpObjectServer::start(&mut config);
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        for statement in SETUP {
            session.success(&mut engine, statement, true);
        }
        assert_eq!(
            data_rows(&session.success(&mut engine, INDEXES, true)),
            EXPECTED,
            "{label}"
        );
        engine.commit_wal().unwrap();
        drop((session, engine, budget));
        for checkpoint in [false, true] {
            std::fs::remove_dir_all(&config.data_dir).unwrap();
            let mut cold_budget = test_engine_budget(&config, 1 << 29);
            let mut recovered = Engine::new(&config, &mut cold_budget).unwrap();
            let mut verifier = ConfiguredTransactionSession::new(&config, &mut cold_budget);
            assert_eq!(
                data_rows(&verifier.success(&mut recovered, INDEXES, true)),
                EXPECTED,
                "{label}: checkpoint={checkpoint}"
            );
            for duplicate in [
                "INSERT INTO pip VALUES (1, 1)",
                "INSERT INTO pip VALUES (11, 1)",
                "INSERT INTO pip VALUES (21, 1)",
            ] {
                assert_eq!(
                    error_sqlstates(&verifier.execute(&mut recovered, duplicate, true)),
                    ["23505"],
                    "{label}: {duplicate}, checkpoint={checkpoint}"
                );
            }
            if !checkpoint {
                assert!(recovered.checkpoint().unwrap());
            }
            recovered.commit_wal().unwrap();
            drop((verifier, recovered, cold_budget));
        }
        drop(server);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }
}

#[test]
fn recovery_rejects_a_valid_partitioned_index_missing_a_partition_copy() {
    let mut config = recovery_config("missing", crate::config::TransactionMode::Mvcc);
    let server = object_reads::HttpObjectServer::start(&mut config);
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    for statement in &SETUP[..3] {
        session.success(&mut engine, statement, true);
    }
    let copy = engine
        .storage
        .index_slot("public", "pip1_a_b_idx", 0)
        .unwrap();
    engine.storage.detach_committed_index_parent_for_test(copy);
    assert!(engine.checkpoint().unwrap());
    engine.commit_wal().unwrap();
    drop((session, engine, budget));
    std::fs::remove_dir_all(&config.data_dir).unwrap();
    let mut cold_budget = test_engine_budget(&config, 1 << 29);
    let error = match Engine::new(&config, &mut cold_budget) {
        Ok(_) => panic!("a partition without its parent index copy must not be served"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains(
            "restored partitioned index \"pip_a\" has 0 attached indexes on partition \"pip1\""
        ),
        "{error}"
    );
    drop(server);
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
}
