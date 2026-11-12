use super::*;

// PostgreSQL 18.6 lists these `reloptions`; see
// tests/external/differential_exact/293_view_reloptions.sql.
const EXPECTED: [&str; 3] = [
    "v1|{check_option=local,security_barrier=true,security_invoker=false}",
    "v2|{security_barrier=true,security_invoker=true,check_option=cascaded}",
    "v3|{security_invoker=false}",
];

/// The written option order and an explicit `security_invoker=false` survive
/// journal replay and checkpoint recovery.
#[test]
fn view_reloptions_order_survives_journal_and_checkpoint_recovery() {
    for checkpoint in [false, true] {
        let name = format!("view-reloptions-{checkpoint}");
        let mut config = test_config(&name);
        config.object_store_on = true;
        config.object_store_sim = true;
        config.wal_upload = true;
        config.wal_upload_sync = true;
        config.object_store_bucket = format!("{name}-{}", std::process::id());
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let query = "SELECT relname, reloptions FROM pg_class \
                     WHERE relname IN ('v1', 'v2', 'v3') ORDER BY 1";
        {
            let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
            session.success(
                &mut engine,
                "CREATE TABLE vt (a int); \
                 CREATE VIEW v1 WITH (check_option=local, security_barrier=true, \
                   security_invoker=false) AS SELECT * FROM vt; \
                 CREATE VIEW v2 WITH (check_option=local, security_barrier=true) \
                   AS SELECT * FROM vt; \
                 ALTER VIEW v2 SET (security_invoker=true, check_option=cascaded); \
                 CREATE VIEW v3 WITH (security_invoker=false) AS SELECT * FROM vt",
                false,
            );
            assert_eq!(
                data_rows(&session.success(&mut engine, query, false)),
                EXPECTED
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
                data_rows(&session.success(&mut engine, query, false)),
                EXPECTED
            );
        }
        std::fs::remove_dir_all(&config.data_dir).unwrap();
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }
}

#[test]
fn view_option_orders_reject_repeated_or_gapped_codes() {
    use crate::sql::ast::ViewOptionName::{CheckOption, SecurityBarrier, SecurityInvoker};
    let order = crate::storage::ViewOptionOrder::EMPTY
        .with_last(CheckOption)
        .with_last(SecurityBarrier)
        .with_last(SecurityInvoker)
        .with_last(CheckOption);
    assert_eq!(
        order.names().collect::<Vec<_>>(),
        [SecurityBarrier, SecurityInvoker, CheckOption]
    );
    assert_eq!(
        crate::storage::ViewOptionOrder::from_code(order.code()),
        Some(order)
    );
    for invalid in [0b0101, 0b0100, 0b1100_0000, 0b0110_1101] {
        assert_eq!(crate::storage::ViewOptionOrder::from_code(invalid), None);
    }
}
