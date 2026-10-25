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
        let mut config = test_config("constraint-report-rows");
        config.transaction_mode = crate::config::TransactionMode::Mvcc;
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        engine.commit_progress = CommitProgressMode::Reactor;
        let mut writer =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(1);
        let mut reader =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(&mut engine, SETUP, true);
        let old = data_rows(&reader.success(
            &mut engine,
            &format!("BEGIN ISOLATION LEVEL {isolation}; {REPORT}"),
            true,
        ));
        assert_eq!(
            old,
            [
                "constraint_child|constraint_child_key|UNIQUE|NO",
                "constraint_child|constraint_child_parent|FOREIGN KEY|NULL",
                "constraint_parent|constraint_parent_key|PRIMARY KEY|NULL",
                "constraint_parent|constraint_child_parent",
                "constraint_parent|constraint_parent_key",
            ]
        );
        writer.success(&mut engine, "ALTER TABLE constraint_parent RENAME TO constraint_renamed_parent; ALTER TABLE constraint_child RENAME TO constraint_renamed_child", true);
        let new = old
            .iter()
            .map(|value| {
                value
                    .replacen("constraint_child|", "constraint_renamed_child|", 1)
                    .replacen("constraint_parent|", "constraint_renamed_parent|", 1)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            data_rows(&reader.success(&mut engine, REPORT, true)),
            if retained { old.clone() } else { new.clone() }
        );
        reader.success(&mut engine, "COMMIT; BEGIN; SAVEPOINT before_rename", true);
        reader.success(
            &mut engine,
            "ALTER TABLE constraint_renamed_parent RENAME TO constraint_private_parent",
            true,
        );
        let private = data_rows(&reader.success(&mut engine, REPORT, true));
        assert!(
            private
                .iter()
                .any(|row| row.starts_with("constraint_private_parent|"))
        );
        reader.success(&mut engine, "ROLLBACK TO before_rename", true);
        assert_eq!(data_rows(&reader.success(&mut engine, REPORT, true)), new);
        reader.success(&mut engine, "ROLLBACK", true);
    }
}

#[test]
fn catalog_constraint_reports_use_postgresql_visibility_and_declared_types() {
    let mut config = test_config("constraint-report-access");
    config.transaction_mode = crate::config::TransactionMode::Mvcc;
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    engine.commit_progress = CommitProgressMode::Reactor;
    let mut owner = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(1);
    let mut reader = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
    owner.success(&mut engine, SETUP, true);
    owner.success(
        &mut engine,
        "CREATE ROLE constraint_reader; GRANT SELECT ON constraint_child TO constraint_reader",
        true,
    );
    reader.success(&mut engine, "SET ROLE constraint_reader", true);
    let count = "SELECT count(*) FROM information_schema.table_constraints WHERE table_name='constraint_child'";
    assert_eq!(data_rows(&reader.success(&mut engine, count, true)), ["0"]);
    assert_eq!(data_rows(&reader.success(&mut engine,
        "SELECT count(*) FROM information_schema.constraint_table_usage WHERE constraint_name='constraint_child_parent'", true)), ["0"]);
    owner.success(
        &mut engine,
        "GRANT UPDATE(y) ON constraint_child TO constraint_reader",
        true,
    );
    assert_eq!(data_rows(&reader.success(&mut engine, count, true)), ["2"]);
    assert_eq!(data_rows(&owner.success(&mut engine,
        "SELECT pg_typeof(constraint_catalog)::text,pg_typeof(constraint_name)::text,
                pg_typeof(constraint_type)::text,pg_typeof(enforced)::text
           FROM information_schema.table_constraints WHERE constraint_name='constraint_child_parent';
         SELECT pg_typeof(table_catalog)::text,pg_typeof(table_name)::text
           FROM information_schema.constraint_table_usage WHERE constraint_name='constraint_child_parent'", true)),
        ["name|name|character varying|character varying", "name|name"]);
}

#[test]
fn catalog_constraint_null_modes_cover_writes_conflicts_like_and_rewrites() {
    for mode in [crate::config::TransactionMode::Mvcc, crate::config::TransactionMode::Locking] {
        let mut config = test_config("constraint-null-modes");
        config.transaction_mode = mode;
        if matches!(mode, crate::config::TransactionMode::Locking) { config.catalog_history_bytes = 0; }
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        engine.commit_progress = CommitProgressMode::Reactor;
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE TABLE null_distinct(a integer UNIQUE NULLS DISTINCT);
             INSERT INTO null_distinct VALUES(NULL),(NULL);
             CREATE TABLE null_equal(a integer,b integer,CONSTRAINT null_equal_key UNIQUE NULLS NOT DISTINCT(a,b));
             INSERT INTO null_equal VALUES(1,NULL)", true);
        assert_eq!(error_sqlstates(&session.execute(&mut engine,
            "INSERT INTO null_equal VALUES(1,NULL)", true)), ["23505"]);
        assert_eq!(data_rows(&session.success(&mut engine,
            "INSERT INTO null_equal VALUES(1,NULL) ON CONFLICT ON CONSTRAINT null_equal_key DO UPDATE SET b=excluded.b RETURNING a;
             SELECT count(*) FROM null_equal", true)), ["1", "1"]);
        session.success(&mut engine,
            "CREATE TABLE null_overlap(a integer UNIQUE,CONSTRAINT null_overlap_strong UNIQUE NULLS NOT DISTINCT(a));
             INSERT INTO null_overlap VALUES(NULL);
             INSERT INTO null_overlap VALUES(NULL) ON CONFLICT(a) DO UPDATE SET a=excluded.a;
             CREATE TABLE null_clone(LIKE null_equal INCLUDING INDEXES);
             INSERT INTO null_clone VALUES(1,NULL)", true);
        assert_eq!(data_rows(&session.success(&mut engine, "SELECT count(*) FROM null_overlap", true)), ["1"]);
        assert_eq!(error_sqlstates(&session.execute(&mut engine,
            "INSERT INTO null_clone VALUES(1,NULL)", true)), ["23505"]);
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT nulls_distinct FROM information_schema.table_constraints
             WHERE table_name='null_clone' AND constraint_type='UNIQUE'", true)), ["NO"]);
        session.success(&mut engine,
            "CREATE TABLE null_rewrite(a integer,b integer);
             INSERT INTO null_rewrite VALUES(1,NULL),(2,NULL)", true);
        assert_eq!(error_sqlstates(&session.execute(&mut engine,
            "ALTER TABLE null_rewrite ALTER COLUMN a TYPE bigint USING 0,
             ADD CONSTRAINT null_rewrite_key UNIQUE NULLS NOT DISTINCT(a,b)", true)), ["23505"]);
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT a FROM null_rewrite ORDER BY a;
             SELECT count(*) FROM information_schema.table_constraints WHERE constraint_name='null_rewrite_key'", true)),
            ["1", "2", "0"]);
    }
}

#[test]
fn catalog_constraint_null_modes_keep_deferred_savepoint_semantics() {
    for mode in [crate::config::TransactionMode::Mvcc, crate::config::TransactionMode::Locking] {
        let mut config = test_config("constraint-null-deferred");
        config.transaction_mode = mode;
        if matches!(mode, crate::config::TransactionMode::Locking) { config.catalog_history_bytes = 0; }
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        engine.commit_progress = CommitProgressMode::Reactor;
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE TABLE null_deferred(a integer CONSTRAINT null_deferred_key UNIQUE NULLS NOT DISTINCT DEFERRABLE INITIALLY DEFERRED);
             BEGIN; INSERT INTO null_deferred VALUES(NULL),(NULL)", true);
        assert_eq!(error_sqlstates(&session.execute(&mut engine, "COMMIT", true)), ["23505"]);
        session.success(&mut engine, "ROLLBACK", true);
        assert_eq!(data_rows(&session.success(&mut engine, "SELECT count(*) FROM null_deferred", true)), ["0"]);
        session.success(&mut engine,
            "BEGIN; INSERT INTO null_deferred VALUES(NULL); SAVEPOINT before_duplicate;
             INSERT INTO null_deferred VALUES(NULL); ROLLBACK TO before_duplicate; COMMIT", true);
        assert_eq!(data_rows(&session.success(&mut engine, "SELECT count(*) FROM null_deferred", true)), ["1"]);
    }
}

#[test]
fn catalog_constraint_null_modes_survive_prepared_decisions_and_empty_disk() {
    for checkpoint in [false, true] {
        for commit in [false, true] {
            let name = format!("constraint-null-recovery-{checkpoint}-{commit}");
            let mut config = test_config(&name);
            config.transaction_mode = crate::config::TransactionMode::Mvcc;
            config.max_prepared_transactions = 1;
            config.object_store_on = true;
            config.object_store_sim = true;
            config.wal_upload = true;
            config.wal_upload_sync = true;
            config.object_store_bucket = format!("{name}-{}", std::process::id());
            crate::object_store::sim::drop_namespace(&config.object_store_bucket);
            let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
            // Simulator uploads own growing fixture vectors. SQL reads and
            // failed writes below still use the production allocation guard.
            session.success(&mut engine,
                "CREATE TABLE saved_nulls(a integer,b integer,CONSTRAINT saved_nulls_key UNIQUE NULLS NOT DISTINCT(a,b));
                 BEGIN; ALTER TABLE saved_nulls RENAME TO saved_renamed_nulls;
                 INSERT INTO saved_renamed_nulls VALUES(1,NULL);
                 PREPARE TRANSACTION 'saved-null-mode'", false);
            if checkpoint { assert!(engine.checkpoint().unwrap()); }
            drop(session);
            drop(engine);
            std::fs::remove_dir_all(&config.data_dir).unwrap();
            let mut cold_budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
            let mut cold = Engine::new(&config, &mut cold_budget)
                .unwrap_or_else(|error| panic!("{name}: {error:?}"));
            let mut session = ConfiguredTransactionSession::new(&config, &mut cold_budget);
            let metadata = "SELECT table_name,nulls_distinct FROM information_schema.table_constraints WHERE constraint_name='saved_nulls_key'";
            assert_eq!(data_rows(&session.success(&mut cold, metadata, true)), ["saved_nulls|NO"]);
            session.success(&mut cold, if commit { "COMMIT PREPARED 'saved-null-mode'" }
                else { "ROLLBACK PREPARED 'saved-null-mode'" }, false);
            let table = if commit { "saved_renamed_nulls" } else { "saved_nulls" };
            assert_eq!(data_rows(&session.success(&mut cold, metadata, true)), [format!("{table}|NO")]);
            if !commit {
                session.success(&mut cold, "INSERT INTO saved_nulls VALUES(1,NULL)", false);
            }
            assert_eq!(error_sqlstates(&session.execute(&mut cold,
                &format!("INSERT INTO {table} VALUES(1,NULL)"), true)), ["23505"]);
            assert_eq!(data_rows(&session.success(&mut cold,
                &format!("SELECT count(*) FROM {table}"), true)), ["1"]);
            drop(session);
            drop(cold);
            crate::object_store::sim::drop_namespace(&config.object_store_bucket);
            std::fs::remove_dir_all(&config.data_dir).unwrap();
        }
    }
}
