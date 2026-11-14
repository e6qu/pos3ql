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

/// A parent key backed only by `CREATE UNIQUE INDEX` is a referenced key like
/// a constraint's, chosen as PostgreSQL's `transformFkeyCheckAttrs` does, and
/// its checks still probe the parent instead of scanning it.
#[test]
fn foreign_keys_reference_plain_unique_indexes() {
    let (mut engine, mut budget) = test_engine();
    let mut run = |sql: &str| run_with(&mut engine, &mut budget, sql);
    for sql in [
        "CREATE TABLE p (a int, b int, c int)",
        "CREATE UNIQUE INDEX p_partial ON p (a) WHERE a > 0",
        "CREATE UNIQUE INDEX p_expression ON p ((a + 1))",
        "CREATE INDEX p_plain ON p (a)",
        "CREATE TABLE c (a int REFERENCES p (a))",
        "ALTER TABLE p ADD CONSTRAINT p_deferred UNIQUE (b) DEFERRABLE",
        "CREATE TABLE c (b int REFERENCES p (b))",
        "CREATE TABLE c (a int, FOREIGN KEY (a, a) REFERENCES p (a, a))",
    ] {
        let output = run(sql);
        let expected: &[&str] = match sql {
            "CREATE TABLE c (b int REFERENCES p (b))" => &["55000"],
            sql if sql.starts_with("CREATE TABLE c") => &["42830"],
            _ => &[],
        };
        assert_eq!(error_sqlstates(&output), expected, "{sql}");
    }
    for sql in [
        "CREATE UNIQUE INDEX p_ba ON p (b, a) NULLS NOT DISTINCT",
        "CREATE UNIQUE INDEX p_a ON p (a) INCLUDE (c)",
        "ALTER TABLE p ADD CONSTRAINT p_ab UNIQUE (a, b)",
        "CREATE TABLE c (a int REFERENCES p (a), b int, FOREIGN KEY (a, b) REFERENCES p (a, b))",
        "INSERT INTO p SELECT g, g, g FROM generate_series(1, 400) g",
    ] {
        assert_eq!(error_sqlstates(&run(sql)), [] as [&str; 0], "{sql}");
    }
    assert_eq!(
        data_rows(&run(
            "SELECT conname, conindid::regclass FROM pg_constraint \
             WHERE conrelid = 'c'::regclass ORDER BY conname"
        )),
        ["c_a_b_fkey|p_ba", "c_a_fkey|p_a"]
    );
    let before = visited();
    let output = run("INSERT INTO c SELECT g, g FROM generate_series(1, 100) g");
    assert_eq!(error_sqlstates(&output), [] as [&str; 0]);
    let probed = visited() - before;
    assert!(probed <= 4 * 100, "{probed} parent rows");
    assert_eq!(
        error_sqlstates(&run("INSERT INTO c VALUES (401, 1)")),
        ["23503"]
    );
}
