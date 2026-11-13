use super::*;

// PostgreSQL 18.6 lists these `reloptions`; see
// tests/external/differential_exact/293_view_reloptions.sql.
const EXPECTED: [&str; 3] = [
    "v1|{check_option=LOCAL,security_barrier=on,security_invoker=0}",
    "v2|{security_barrier=true,security_invoker=yes,check_option=cascaded}",
    "v3|{security_invoker=false}",
];

/// The written option order and spellings, and an explicit
/// `security_invoker=false`, survive journal replay and checkpoint recovery.
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
                 CREATE VIEW v1 WITH (check_option='LOCAL', security_barrier=on, \
                   security_invoker=0) AS SELECT * FROM vt; \
                 CREATE VIEW v2 WITH (check_option=local, security_barrier) \
                   AS SELECT * FROM vt; \
                 ALTER VIEW v2 SET (security_invoker='yes', check_option=cascaded); \
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
fn view_option_lists_reject_repeated_gapped_or_unspelled_options() {
    use crate::sql::ast::ViewOptionName::{CheckOption, SecurityBarrier, SecurityInvoker};
    use crate::storage::{ReloptionSpelling, ViewOptionOrder};
    let spelled = |text| ReloptionSpelling::new(text).unwrap();
    let order = ViewOptionOrder::EMPTY
        .with_last(CheckOption, spelled("LOCAL"))
        .with_last(SecurityBarrier, spelled("on"))
        .with_last(SecurityInvoker, spelled("1"))
        .with_last(CheckOption, spelled("cascaded"));
    assert_eq!(
        order.names().collect::<Vec<_>>(),
        [SecurityBarrier, SecurityInvoker, CheckOption]
    );
    assert_eq!(order.spelling(CheckOption), "cascaded");
    assert_eq!(ViewOptionOrder::decode(order.encode()), Some(order));
    for invalid in [0b0101, 0b0100, 0b1100_0000, 0b0110_1101] {
        let mut bytes = order.encode();
        bytes[0] = invalid;
        assert_eq!(ViewOptionOrder::decode(bytes), None);
    }
    // A listed option needs its spelling, and an unlisted one has none.
    let mut bytes = order.without(SecurityBarrier).encode();
    bytes[0] = order.encode()[0];
    assert_eq!(ViewOptionOrder::decode(bytes), None);
    let mut bytes = order.encode();
    bytes[0] = order.without(SecurityBarrier).encode()[0];
    assert_eq!(ViewOptionOrder::decode(bytes), None);
    for text in ["", " on", "tooolong!", "o n"] {
        assert_eq!(ReloptionSpelling::new(text), None, "{text:?}");
    }
}
