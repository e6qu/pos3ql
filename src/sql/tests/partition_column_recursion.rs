//! ALTER TABLE column changes reach every partition level, and each column's
//! `attislocal` survives journal replay and checkpoint recovery.

use super::*;

const SETUP: &str = "CREATE TABLE pr (a int, b int) PARTITION BY RANGE (a); \
     CREATE TABLE pr_1 PARTITION OF pr FOR VALUES FROM (0) TO (10) PARTITION BY RANGE (a); \
     CREATE TABLE pr_11 PARTITION OF pr_1 FOR VALUES FROM (0) TO (5); \
     CREATE TABLE pr_2 (a int, b int); \
     ALTER TABLE pr ATTACH PARTITION pr_2 FOR VALUES FROM (10) TO (20); \
     CREATE TABLE ih (a int); CREATE TABLE ih_c (a int, c int) INHERITS (ih); \
     INSERT INTO pr VALUES (1, 1), (12, 3); \
     ALTER TABLE pr ADD COLUMN c text DEFAULT 'x'; \
     ALTER TABLE pr RENAME COLUMN b TO bb; \
     ALTER TABLE pr ALTER COLUMN bb TYPE bigint";

const ATTRIBUTES: &str = "SELECT c.relname, a.attname, format_type(a.atttypid, a.atttypmod), \
     a.attislocal, a.attinhcount FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
     WHERE c.relname IN ('pr', 'pr_1', 'pr_11', 'pr_2', 'ih', 'ih_c') AND a.attnum > 0 \
     AND NOT a.attisdropped ORDER BY c.relname, a.attnum";

const EXPECTED: [&str; 15] = [
    "ih|a|integer|t|0",
    "ih_c|a|integer|t|1",
    "ih_c|c|integer|t|0",
    "pr|a|integer|t|0",
    "pr|bb|bigint|t|0",
    "pr|c|text|t|0",
    "pr_1|a|integer|f|1",
    "pr_1|bb|bigint|f|1",
    "pr_1|c|text|f|1",
    "pr_11|a|integer|f|1",
    "pr_11|bb|bigint|f|1",
    "pr_11|c|text|f|1",
    "pr_2|a|integer|f|1",
    "pr_2|bb|bigint|f|1",
    "pr_2|c|text|f|1",
];

fn assert_shape(engine: &mut Engine, budget: &mut Budget) {
    assert_eq!(data_rows(&run_with(engine, budget, ATTRIBUTES)), EXPECTED);
    assert_eq!(
        data_rows(&run_with(
            engine,
            budget,
            "SELECT tableoid::regclass, a, bb, c FROM pr ORDER BY a"
        )),
        ["pr_11|1|1|x", "pr_2|12|3|x"]
    );
}

#[test]
fn partition_column_changes_survive_journal_replay() {
    let config = test_config("partition-column-replay");
    {
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 28));
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        run_with(&mut engine, &mut budget, SETUP);
        assert_shape(&mut engine, &mut budget);
        engine.commit_wal().unwrap();
    }
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 28));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    assert_shape(&mut engine, &mut budget);
    // The recovered shape accepts a constraint over the added column.
    let output = run_with(
        &mut engine,
        &mut budget,
        "ALTER TABLE pr ADD CONSTRAINT pr_c CHECK (c <> '')",
    );
    assert!(!String::from_utf8_lossy(&output).contains("ERROR"));
}

#[test]
fn partition_column_changes_survive_checkpoint_recovery() {
    let mut config = test_config("partition-column-checkpoint");
    config.object_store_on = true;
    config.object_store_sim = true;
    config.object_store_bucket = format!("partition-column-checkpoint-{}", std::process::id());
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    {
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        run_with(&mut engine, &mut budget, SETUP);
        assert!(engine.checkpoint().unwrap());
    }
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    assert_shape(&mut engine, &mut budget);
}
