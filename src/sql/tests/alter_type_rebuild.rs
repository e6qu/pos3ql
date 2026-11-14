use super::*;

const SETUP: &[&str] = &[
    "CREATE TABLE atr_p (id int PRIMARY KEY, u int UNIQUE)",
    "INSERT INTO atr_p VALUES (1, 1), (2, 2)",
    "CREATE TABLE atr_t (a int, b int, c int, CONSTRAINT atr_t_u UNIQUE (b), \
     CONSTRAINT atr_t_chk CHECK (b > 0), CONSTRAINT atr_t_chk2 CHECK (a > 0), \
     CONSTRAINT atr_t_pk PRIMARY KEY (a, b), \
     CONSTRAINT atr_t_fk FOREIGN KEY (b) REFERENCES atr_p (u))",
    "CREATE INDEX atr_t_bi ON atr_t (b)",
    "CREATE INDEX atr_t_ci ON atr_t (c)",
    "CREATE STATISTICS atr_t_s ON a, b FROM atr_t",
    "CREATE TABLE atr_r (x int REFERENCES atr_t (b))",
    "INSERT INTO atr_t VALUES (1, 1, 1)",
    "CREATE TABLE atr_pr (id int, label int, CONSTRAINT atr_pr_pk PRIMARY KEY (id, label)) \
     PARTITION BY LIST (id)",
    "CREATE TABLE atr_pr_1 PARTITION OF atr_pr FOR VALUES IN (1)",
    "CREATE INDEX atr_pr_label_idx ON atr_pr (label)",
    "ALTER INDEX atr_pr_1_label_idx RENAME TO atr_pr_1_renamed",
    "ALTER TABLE atr_pr_1 RENAME CONSTRAINT atr_pr_1_pkey TO atr_pr_1_custom",
    "ALTER TABLE atr_t ALTER COLUMN b TYPE bigint",
    "ALTER TABLE atr_pr ALTER COLUMN label TYPE bigint",
];

/// Constraints, indexes and statistics in OID order, per catalog.
const ORDER: &str = "SELECT conname FROM pg_constraint \
     WHERE conrelid::regclass::text LIKE 'atr%' AND contype <> 'n' ORDER BY oid";
const INDEXES: &str = "SELECT c.relname, coalesce(p.relname, '-') FROM pg_class c \
     LEFT JOIN pg_inherits h ON h.inhrelid = c.oid \
     LEFT JOIN pg_class p ON p.oid = h.inhparent \
     WHERE c.relname LIKE 'atr%' AND c.relkind IN ('i', 'I') ORDER BY c.oid";

const EXPECTED_ORDER: &[&str] = &[
    "atr_p_pkey",
    "atr_p_u_key",
    "atr_t_chk2",
    "atr_t_pk",
    "atr_t_u",
    "atr_t_chk",
    "atr_t_fk",
    "atr_r_x_fkey",
    "atr_pr_pk",
    "atr_pr_1_pkey",
];
const EXPECTED_INDEXES: &[&str] = &[
    "atr_p_pkey|-",
    "atr_p_u_key|-",
    "atr_t_ci|-",
    "atr_t_pk|-",
    "atr_t_u|-",
    "atr_t_bi|-",
    "atr_pr_pk|-",
    "atr_pr_1_pkey|atr_pr_pk",
    "atr_pr_label_idx|-",
    "atr_pr_1_label_idx|atr_pr_label_idx",
];

fn recovery_config(label: &str) -> Config {
    let mut config = test_config(&format!("alter-type-rebuild-{label}"));
    config.object_store_on = true;
    config.object_store_bucket = format!("alter-type-rebuild-{label}-{}", std::process::id());
    config.wal_upload = true;
    config.wal_upload_sync = true;
    config.block_cache_bytes = 0;
    config.disk_cache_bytes = 0;
    // A rebuilt index's new binding exists beside the old one until commit.
    config.max_value_indexes = 64;
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    config
}

fn verify(engine: &mut Engine, session: &mut ConfiguredTransactionSession, context: &str) {
    assert_eq!(
        data_rows(&session.success(engine, ORDER, true)),
        EXPECTED_ORDER,
        "{context}"
    );
    assert_eq!(
        data_rows(&session.success(engine, INDEXES, true)),
        EXPECTED_INDEXES,
        "{context}"
    );
    for (statement, sqlstate) in [
        ("INSERT INTO atr_t VALUES (2, 1, 2)", "23505"),
        ("INSERT INTO atr_t VALUES (3, 7, 3)", "23503"),
        ("INSERT INTO atr_t VALUES (3, -2, 3)", "23514"),
        ("INSERT INTO atr_r VALUES (9)", "23503"),
        ("INSERT INTO atr_pr VALUES (1, 1), (1, 1)", "23505"),
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(engine, statement, true)),
            [sqlstate],
            "{context}: {statement}"
        );
    }
}

#[test]
fn alter_column_type_rebuilds_dependents_in_postgresql_order_through_cold_recovery() {
    let mut config = recovery_config("order");
    let server = object_reads::HttpObjectServer::start(&mut config);
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    for statement in SETUP {
        session.success(&mut engine, statement, true);
    }
    verify(&mut engine, &mut session, "live");
    engine.commit_wal().unwrap();
    drop((session, engine, budget));
    for checkpoint in [false, true] {
        std::fs::remove_dir_all(&config.data_dir).unwrap();
        let mut cold_budget = test_engine_budget(&config, 1 << 29);
        let mut recovered = Engine::new(&config, &mut cold_budget).unwrap();
        let mut verifier = ConfiguredTransactionSession::new(&config, &mut cold_budget);
        verify(
            &mut recovered,
            &mut verifier,
            &format!("checkpoint={checkpoint}"),
        );
        if !checkpoint {
            assert!(recovered.checkpoint().unwrap());
        }
        recovered.commit_wal().unwrap();
        drop((verifier, recovered, cold_budget));
    }
    drop(server);
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
}

#[test]
fn a_partitioned_index_recreated_in_one_transaction_keeps_its_partition_copies_in_recovery() {
    let mut config = recovery_config("recreate");
    let server = object_reads::HttpObjectServer::start(&mut config);
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    for statement in [
        "CREATE TABLE atr_x (id int, label int) PARTITION BY LIST (id)",
        "CREATE TABLE atr_x1 PARTITION OF atr_x FOR VALUES IN (1)",
        "CREATE UNIQUE INDEX atr_x_idx ON atr_x (id, label)",
        "BEGIN",
        "DROP INDEX atr_x_idx",
        "CREATE UNIQUE INDEX atr_x_idx ON atr_x (id, label)",
        "COMMIT",
    ] {
        session.success(&mut engine, statement, true);
    }
    engine.commit_wal().unwrap();
    drop((session, engine, budget));
    std::fs::remove_dir_all(&config.data_dir).unwrap();
    let mut cold_budget = test_engine_budget(&config, 1 << 29);
    let mut recovered = Engine::new(&config, &mut cold_budget).unwrap();
    let mut verifier = ConfiguredTransactionSession::new(&config, &mut cold_budget);
    assert_eq!(
        error_sqlstates(&verifier.execute(
            &mut recovered,
            "INSERT INTO atr_x VALUES (1, 1), (1, 1)",
            true
        )),
        ["23505"]
    );
    drop((verifier, recovered, cold_budget, server));
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
}
