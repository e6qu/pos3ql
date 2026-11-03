use super::*;

#[test]
fn expression_source_positions_do_not_change_grouping_or_ordering_identity() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("expression-source-position-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT abs(-(v+0))::integer,count(*) FROM (VALUES(1),(2),(1)) input(v) \
             GROUP BY abs(-(v+0))::integer ORDER BY abs(-(v+0))::integer",
                true
            )),
            ["1|2", "2|1"]
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT abs(-(v+0))::integer FROM (VALUES(2),(1),(1)) input(v) \
             ORDER BY abs(-(v+0))::integer",
                true
            )),
            ["1", "1", "2"]
        );
    }
}
