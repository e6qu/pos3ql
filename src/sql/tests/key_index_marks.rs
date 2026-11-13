use super::*;

// PostgreSQL 18.6 returns these rows; see
// tests/external/differential_exact/341_key_index_cluster_and_replica_identity.sql
// and 340_access_methods_domains_and_initdb_acls.sql.
const MARKS: [&str; 4] = [
    "km_b_key|f|t",
    "km_c_d_key|f|f",
    "km_c_idx|f|f",
    "km_pkey|t|f",
];
const ACCESS_METHODS: [&str; 2] = ["kp|0", "kp_1|2"];
const EXCLUSIONS: &str = "SELECT conname, conexclop, pg_get_constraintdef(oid) \
                          FROM pg_constraint WHERE conrelid = 'ke'::regclass ORDER BY 1";

/// CLUSTER and replica identity marks on key indexes, a partitioned table's
/// unset access method and exclusion constraints' methods and operators
/// survive journal replay and checkpoint recovery.
#[test]
fn key_index_marks_and_unset_access_methods_survive_recovery() {
    for checkpoint in [false, true] {
        let name = format!("key-index-marks-{checkpoint}");
        let mut config = test_config(&name);
        config.object_store_on = true;
        config.object_store_sim = true;
        config.wal_upload = true;
        config.wal_upload_sync = true;
        config.max_value_indexes = 16;
        config.object_store_bucket = format!("{name}-{}", std::process::id());
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let marks = "SELECT c.relname, i.indisclustered, i.indisreplident FROM pg_index i \
                     JOIN pg_class c ON c.oid = i.indexrelid \
                     WHERE i.indrelid = 'km'::regclass ORDER BY 1";
        let access_methods = "SELECT relname, relam FROM pg_class \
                              WHERE relname IN ('kp', 'kp_1') ORDER BY 1";
        {
            let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
            session.success(
                &mut engine,
                "CREATE TABLE km (a int PRIMARY KEY, b int NOT NULL UNIQUE, c int, \
                   d int NOT NULL, UNIQUE (c, d)); \
                 CREATE INDEX km_c_idx ON km (c); \
                 ALTER TABLE km CLUSTER ON km_c_idx; \
                 ALTER TABLE km CLUSTER ON km_pkey; \
                 ALTER TABLE km REPLICA IDENTITY USING INDEX km_b_key; \
                 CREATE TABLE kp (a int) PARTITION BY LIST (a); \
                 CREATE TABLE kp_1 PARTITION OF kp FOR VALUES IN (1); \
                 CREATE TABLE ke (b box, t text, EXCLUDE USING gist (b WITH &&), \
                   EXCLUDE (t WITH =))",
                false,
            );
            assert_eq!(
                data_rows(&session.success(&mut engine, marks, false)),
                MARKS
            );
            if checkpoint {
                assert!(engine.checkpoint().unwrap());
            }
            engine.commit_wal().unwrap();
        }
        std::fs::remove_dir_all(&config.data_dir).unwrap();
        {
            let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
            assert_eq!(
                data_rows(&session.success(&mut engine, marks, false)),
                MARKS
            );
            assert_eq!(
                data_rows(&session.success(&mut engine, access_methods, false)),
                ACCESS_METHODS
            );
            assert_eq!(
                data_rows(&session.success(
                    &mut engine,
                    "SELECT relreplident FROM pg_class WHERE relname = 'km'",
                    false
                )),
                ["i"]
            );
            assert_eq!(
                data_rows(&session.success(&mut engine, EXCLUSIONS, false)),
                [
                    "ke_b_excl|{500}|EXCLUDE USING gist (b WITH &&)",
                    "ke_t_excl|{98}|EXCLUDE USING btree (t WITH =)",
                ]
            );
            let output = session.execute(
                &mut engine,
                "INSERT INTO ke VALUES ('((0,0),(2,2))', 'a'), ('((1,1),(3,3))', 'b')",
                false,
            );
            assert!(
                String::from_utf8_lossy(&output).contains("C23P01"),
                "{}",
                String::from_utf8_lossy(&output)
            );
        }
        std::fs::remove_dir_all(&config.data_dir).unwrap();
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }
}
