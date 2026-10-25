use super::*;

const SETUP: &str = "
    CREATE TABLE constraint_parent(a integer, b integer,
        CONSTRAINT constraint_parent_key PRIMARY KEY(a,b));
    CREATE TABLE constraint_child(x integer, y integer,
        CONSTRAINT constraint_child_key UNIQUE NULLS NOT DISTINCT(x,y),
        CONSTRAINT constraint_child_parent FOREIGN KEY(x,y) REFERENCES constraint_parent(b,a));
";

const REPORT: &str = "
    SELECT table_name,constraint_name,constraint_type,nulls_distinct
      FROM information_schema.table_constraints
      WHERE constraint_name IN ('constraint_parent_key','constraint_child_key','constraint_child_parent')
      ORDER BY constraint_name;
    SELECT table_name,constraint_name FROM information_schema.constraint_table_usage
      WHERE constraint_name='constraint_child_parent';
    SELECT tablename,indexname FROM pg_indexes WHERE indexname='constraint_parent_key';
";

#[test]
fn catalog_constraint_reports_keep_selected_names_and_private_edits() {
    for (isolation, retained) in [("READ COMMITTED", false), ("REPEATABLE READ", true)] {
        let config = test_config("constraint-report-rows");
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        engine.commit_progress = CommitProgressMode::Reactor;
        let mut writer = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(1);
        let mut reader = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(&mut engine, SETUP, true);
        let old = data_rows(&reader.success(&mut engine, &format!("BEGIN ISOLATION LEVEL {isolation}; {REPORT}"), true));
        assert_eq!(old, [
            "constraint_child|constraint_child_key|UNIQUE|NO",
            "constraint_child|constraint_child_parent|FOREIGN KEY|NULL",
            "constraint_parent|constraint_parent_key|PRIMARY KEY|NULL",
            "constraint_parent|constraint_child_parent",
            "constraint_parent|constraint_parent_key",
        ]);
        writer.success(&mut engine, "ALTER TABLE constraint_parent RENAME TO constraint_renamed_parent; ALTER TABLE constraint_child RENAME TO constraint_renamed_child", true);
        let new = old.iter().map(|value| value.replacen("constraint_child|", "constraint_renamed_child|", 1)
            .replacen("constraint_parent|", "constraint_renamed_parent|", 1)).collect::<Vec<_>>();
        assert_eq!(data_rows(&reader.success(&mut engine, REPORT, true)), if retained { old.clone() } else { new.clone() });
        reader.success(&mut engine, "COMMIT; BEGIN; SAVEPOINT before_rename", true);
        reader.success(&mut engine, "ALTER TABLE constraint_renamed_parent RENAME TO constraint_private_parent", true);
        let private = data_rows(&reader.success(&mut engine, REPORT, true));
        assert!(private.iter().any(|row| row.starts_with("constraint_private_parent|")));
        reader.success(&mut engine, "ROLLBACK TO before_rename", true);
        assert_eq!(data_rows(&reader.success(&mut engine, REPORT, true)), new);
        reader.success(&mut engine, "ROLLBACK", true);
    }
}

#[test]
fn catalog_constraint_reports_use_postgresql_visibility_and_declared_types() {
    let config = test_config("constraint-report-access");
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    engine.commit_progress = CommitProgressMode::Reactor;
    let mut owner = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(1);
    let mut reader = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
    owner.success(&mut engine, SETUP, true);
    owner.success(&mut engine, "CREATE ROLE constraint_reader; GRANT SELECT ON constraint_child TO constraint_reader", true);
    reader.success(&mut engine, "SET ROLE constraint_reader", true);
    let count = "SELECT count(*) FROM information_schema.table_constraints WHERE table_name='constraint_child'";
    assert_eq!(data_rows(&reader.success(&mut engine, count, true)), ["0"]);
    assert_eq!(data_rows(&reader.success(&mut engine,
        "SELECT count(*) FROM information_schema.constraint_table_usage WHERE constraint_name='constraint_child_parent'", true)), ["0"]);
    owner.success(&mut engine, "GRANT UPDATE(y) ON constraint_child TO constraint_reader", true);
    assert_eq!(data_rows(&reader.success(&mut engine, count, true)), ["2"]);
    assert_eq!(data_rows(&owner.success(&mut engine,
        "SELECT pg_typeof(constraint_catalog)::text,pg_typeof(constraint_name)::text,
                pg_typeof(constraint_type)::text,pg_typeof(enforced)::text
           FROM information_schema.table_constraints WHERE constraint_name='constraint_child_parent';
         SELECT pg_typeof(table_catalog)::text,pg_typeof(table_name)::text
           FROM information_schema.constraint_table_usage WHERE constraint_name='constraint_child_parent'", true)),
        ["name|name|character varying|character varying", "name|name"]);
}
