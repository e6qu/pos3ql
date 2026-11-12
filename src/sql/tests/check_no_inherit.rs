use super::*;

// PostgreSQL 18.6 lists these rows; see
// tests/external/differential_exact/296_check_no_inherit.sql.
const EXPECTED: [&str; 4] = [
    "np|pc|t|t|0|CHECK ((a > 0)) NO INHERIT",
    "np|qc|f|t|0|CHECK ((a < 100))",
    "np|rc|t|t|0|CHECK ((a <> 5)) NO INHERIT NOT VALID",
    "nc|qc|f|f|1|CHECK ((a < 100))",
];

/// A `NO INHERIT` CHECK keeps its table-local state through journal replay and
/// checkpoint recovery, so a child created afterwards still lacks it.
#[test]
fn check_no_inherit_survives_journal_and_checkpoint_recovery() {
    for checkpoint in [false, true] {
        let name = format!("check-no-inherit-{checkpoint}");
        let mut config = test_config(&name);
        config.object_store_on = true;
        config.object_store_sim = true;
        config.wal_upload = true;
        config.wal_upload_sync = true;
        config.object_store_bucket = format!("{name}-{}", std::process::id());
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let query = "SELECT conrelid::regclass, conname, connoinherit, conislocal, coninhcount, \
                     pg_get_constraintdef(oid) FROM pg_constraint \
                     WHERE conrelid::regclass::text IN ('np', 'nc') AND contype = 'c' \
                     ORDER BY conrelid, conname";
        {
            let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
            session.success(
                &mut engine,
                "CREATE TABLE np (a int, CONSTRAINT pc CHECK (a > 0) NO INHERIT, \
                   CONSTRAINT qc CHECK (a < 100)); \
                 ALTER TABLE np ADD CONSTRAINT rc CHECK (a <> 5) NO INHERIT NOT VALID",
                false,
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
            session.success(&mut engine, "CREATE TABLE nc () INHERITS (np)", false);
            assert_eq!(
                data_rows(&session.success(&mut engine, query, false)),
                EXPECTED
            );
            assert_eq!(
                error_sqlstates(&session.execute(&mut engine, "INSERT INTO np VALUES (-1)", false)),
                ["23514"]
            );
            session.success(&mut engine, "INSERT INTO nc VALUES (-1)", false);
        }
        std::fs::remove_dir_all(&config.data_dir).unwrap();
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }
}
