//! Foreign key checks find their parent row through the parent's unique key
//! instead of scanning the parent table.

use super::*;
use crate::sql::exec::constraints::PARENT_ROWS_VISITED;

fn visited() -> u64 {
    PARENT_ROWS_VISITED.with(core::cell::Cell::get)
}

/// Each checked child row examines a bounded number of parent rows, whether
/// the parent's rows are committed or the transaction's own, so a 500-row
/// insert into a child of a 2000-row parent does not read the parent 500
/// times; both transaction modes, as fixture 226 runs them.
#[test]
fn foreign_key_checks_probe_the_parent_key() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("foreign-key-lookups-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(
            &mut engine,
            "CREATE TABLE big(id int PRIMARY KEY); \
             INSERT INTO big SELECT generate_series(1, 2000); \
             CREATE TABLE bigc(id int, pid int REFERENCES big)",
            false,
        );
        let before = visited();
        session.success(
            &mut engine,
            "INSERT INTO bigc SELECT g, g FROM generate_series(1, 500) g",
            false,
        );
        let committed = visited() - before;
        assert!(committed <= 2 * 500, "{mode:?}: {committed} parent rows");
        // Parents the transaction itself inserted are pending changes.
        let before = visited();
        session.success(
            &mut engine,
            "BEGIN; INSERT INTO big SELECT generate_series(2001, 2010); \
             INSERT INTO bigc SELECT g, g FROM generate_series(2001, 2010) g; COMMIT",
            false,
        );
        let own = visited() - before;
        assert!(own <= 10 * 12, "{mode:?}: {own} parent rows");
        let output = session.execute(&mut engine, "INSERT INTO bigc VALUES (0, 2011)", false);
        assert_eq!(error_sqlstates(&output), ["23503"], "{mode:?}");
        assert_eq!(
            data_rows(&session.success(&mut engine, "SELECT count(*) FROM bigc", true)),
            ["510"],
            "{mode:?}"
        );
    }
}
