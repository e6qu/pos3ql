use super::*;

const SETUP: &str = "
    CREATE TABLE derived_source(id integer, payload text);
    CREATE VIEW derived_view AS SELECT id, payload FROM derived_source WITH LOCAL CHECK OPTION;
    CREATE MATERIALIZED VIEW derived_materialized AS SELECT id FROM derived_source WITH NO DATA;
    CREATE RULE derived_rule AS ON INSERT TO derived_source DO ALSO NOTHING;
    CREATE SEQUENCE derived_sequence AS integer START 3 INCREMENT 2;
";

const REPORT: &str = "
    SELECT viewname FROM pg_views WHERE viewname IN ('derived_view','derived_renamed_view');
    SELECT table_name, check_option FROM information_schema.views
      WHERE table_name IN ('derived_view','derived_renamed_view');
    SELECT matviewname,hasindexes,ispopulated FROM pg_matviews
      WHERE matviewname IN ('derived_materialized','derived_renamed_materialized');
    SELECT rulename,tablename FROM pg_rules WHERE rulename IN ('derived_rule','derived_renamed_rule');
    SELECT sequencename,increment_by,pg_typeof(data_type)::text FROM pg_sequences
      WHERE sequencename IN ('derived_sequence','derived_renamed_sequence');
    SELECT sequence_name, increment FROM information_schema.sequences
      WHERE sequence_name IN ('derived_sequence','derived_renamed_sequence');
    SELECT view_name,table_name FROM information_schema.view_table_usage
      WHERE view_name IN ('derived_view','derived_renamed_view');
    SELECT view_name,table_name,column_name FROM information_schema.view_column_usage
      WHERE view_name IN ('derived_view','derived_renamed_view') ORDER BY column_name;
";

const OLD: [&str; 9] = [
    "derived_view", "derived_view|LOCAL", "derived_materialized|f|f",
    "derived_rule|derived_source", "derived_sequence|2|regtype",
    "derived_sequence|2", "derived_view|derived_source",
    "derived_view|derived_source|id", "derived_view|derived_source|payload",
];

const NEW: [&str; 9] = [
    "derived_renamed_view", "derived_renamed_view|CASCADED", "derived_renamed_materialized|t|f",
    "derived_renamed_rule|derived_renamed_source", "derived_renamed_sequence|5|regtype",
    "derived_renamed_sequence|5", "derived_renamed_view|derived_renamed_source",
    "derived_renamed_view|derived_renamed_source|id", "derived_renamed_view|derived_renamed_source|renamed_payload",
];

const CHANGE: &str = "
    ALTER VIEW derived_view RENAME TO derived_renamed_view;
    ALTER VIEW derived_renamed_view SET (check_option=cascaded);
    ALTER MATERIALIZED VIEW derived_materialized RENAME TO derived_renamed_materialized;
    CREATE INDEX derived_materialized_index ON derived_renamed_materialized(id);
    ALTER RULE derived_rule ON derived_source RENAME TO derived_renamed_rule;
    ALTER TABLE derived_source RENAME TO derived_renamed_source;
    ALTER TABLE derived_renamed_source RENAME payload TO renamed_payload;
    ALTER SEQUENCE derived_sequence RENAME TO derived_renamed_sequence;
    ALTER SEQUENCE derived_renamed_sequence INCREMENT 5;
";

#[test]
fn catalog_snapshot_derived_reports_preserve_rows_without_freezing_current_values() {
    for (isolation, retained) in [("REPEATABLE READ", OLD), ("READ COMMITTED", NEW)] {
        let config = test_config("derived-catalog-reports");
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        engine.commit_progress = CommitProgressMode::Reactor;
        let mut writer = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(1);
        let mut reader = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(&mut engine, SETUP, true);
        let initial = reader.success(&mut engine, &format!("BEGIN ISOLATION LEVEL {isolation}; {REPORT}"), true);
        assert_eq!(data_rows(&initial), OLD);
        writer.success(&mut engine, CHANGE, true);
        writer.success(&mut engine, "SELECT nextval('derived_renamed_sequence')", true);
        let selected = reader.success(&mut engine, REPORT, true);
        assert_eq!(data_rows(&selected), retained, "{isolation}: {}", String::from_utf8_lossy(&selected));
        let live = reader.success(&mut engine,
            "SELECT last_value FROM pg_sequences WHERE sequencename IN ('derived_sequence','derived_renamed_sequence');
             SELECT to_regclass('derived_view') IS NULL, to_regclass('derived_renamed_view') IS NOT NULL;
             COMMIT;", true);
        assert_eq!(data_rows(&live), ["3", "t|t"]);
        assert_eq!(data_rows(&reader.success(&mut engine, REPORT, true)), NEW);
    }
}

#[test]
fn catalog_snapshot_derived_reports_keep_private_edits_and_savepoint_rollback() {
    for mode in [crate::config::TransactionMode::Mvcc, crate::config::TransactionMode::Locking] {
        let mut config = test_config("private-derived-catalog-reports");
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        engine.commit_progress = CommitProgressMode::Reactor;
        let mut owner = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(1);
        let mut observer = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        owner.success(&mut engine, SETUP, true);
        owner.success(&mut engine, "BEGIN; SAVEPOINT report_change", true);
        owner.success(&mut engine, CHANGE, true);
        assert_eq!(data_rows(&owner.success(&mut engine, REPORT, true)), NEW);
        assert_eq!(data_rows(&observer.success(&mut engine, REPORT, true)), OLD);
        owner.success(&mut engine, "ROLLBACK TO report_change", true);
        assert_eq!(data_rows(&owner.success(&mut engine, REPORT, true)), OLD);
        owner.success(&mut engine, CHANGE, true);
        owner.success(&mut engine, "ROLLBACK", true);
        assert_eq!(data_rows(&observer.success(&mut engine, REPORT, true)), OLD);
    }
}

#[test]
fn catalog_snapshot_derived_usage_is_distinct_and_checks_referenced_ownership() {
    let config = test_config("derived-catalog-privileges");
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    engine.commit_progress = CommitProgressMode::Reactor;
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "
        CREATE ROLE derived_reader;
        CREATE TABLE derived_private(id integer, payload text);
        CREATE VIEW derived_repeated AS SELECT a.id, b.id AS other, a.payload
          FROM derived_private a JOIN derived_private b ON a.id=b.id;
        CREATE SEQUENCE derived_hidden_sequence;
        SELECT nextval('derived_hidden_sequence');
        GRANT SELECT ON derived_repeated TO derived_reader;
    ", true);
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT count(*) FROM information_schema.view_table_usage WHERE view_name='derived_repeated';
         SELECT column_name FROM information_schema.view_column_usage WHERE view_name='derived_repeated' ORDER BY column_name;
         SET ROLE derived_reader;
         SELECT table_name,view_definition IS NULL FROM information_schema.views WHERE table_name='derived_repeated';
         SELECT count(*) FROM information_schema.view_table_usage WHERE view_name='derived_repeated';
         SELECT count(*) FROM information_schema.view_column_usage WHERE view_name='derived_repeated';
         SELECT count(*) FROM information_schema.sequences WHERE sequence_name='derived_hidden_sequence';
         SELECT last_value IS NULL FROM pg_sequences WHERE sequencename='derived_hidden_sequence';
         RESET ROLE;", true)),
        ["1", "id", "payload", "derived_repeated|t", "0", "0", "0", "t"]);
}
