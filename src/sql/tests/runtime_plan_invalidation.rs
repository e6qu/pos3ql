use super::*;

// PostgreSQL 18.6 two-session oracle SQL/Python SHA256:
// 3df4b20befdb50122161465bd83cc7a870e63ccc78bd6b9cb8e8c0bafc444e38,
// c68084448f63edc5dc2d636ec2b7c06264088b554138411ad526332c0379c5b4.
#[test]
fn namespace_plan_invalidations_follow_local_commit_and_rollback_boundaries() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("namespace-plan-invalidation-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut first = ConfiguredTransactionSession::new(&config, &mut budget);
        let mut second =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        let replacement = TxnState::new_with_config(&mut budget, &config).unwrap();
        first.success(
            &mut engine,
            "CREATE SCHEMA global_probe; CREATE SEQUENCE global_checks; \
             CREATE TYPE global_choice AS ENUM('a'); \
             CREATE FUNCTION global_allowed(value global_choice) RETURNS boolean LANGUAGE plpgsql \
             AS $$BEGIN PERFORM nextval('global_checks'); RETURN true; END$$; \
             CREATE DOMAIN global_checked AS global_choice CHECK(global_allowed(VALUE)); \
             CREATE FUNCTION global_body() RETURNS global_checked[] LANGUAGE sql \
             SET search_path=public AS 'SELECT ''{a}''::global_checked[]'",
            true,
        );
        let counter = "SELECT last_value FROM global_checks";
        first.success(&mut engine, "SELECT global_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["2"]);
        first.success(
            &mut engine,
            "CREATE SCHEMA IF NOT EXISTS global_probe; ALTER SCHEMA global_probe OWNER TO postgres",
            true,
        );
        assert_eq!(
            error_sqlstates(&first.execute(&mut engine, "CREATE SCHEMA global_probe", true)),
            ["42P06"]
        );
        first.success(&mut engine, "SELECT global_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["2"]);

        second.success(&mut engine, "SELECT global_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["3"]
        );
        first.success(&mut engine, "BEGIN; CREATE SCHEMA global_pending", true);
        second.success(&mut engine, "SELECT global_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["3"]
        );
        first.success(&mut engine, "SELECT global_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["4"]);
        first.success(&mut engine, "COMMIT; SELECT global_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["5"]);
        second.success(&mut engine, "SELECT global_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["6"]
        );

        first.success(
            &mut engine,
            "BEGIN; CREATE SCHEMA global_rollback; SELECT global_body()",
            true,
        );
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["7"]);
        second.success(&mut engine, "SELECT global_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["7"]
        );
        first.success(&mut engine, "ROLLBACK; SELECT global_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["8"]);
        second.success(&mut engine, "SELECT global_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["8"]
        );

        // A fresh, preallocated transaction workspace keeps this backend's
        // plan lineage; temporary transaction buffers do not define a session.
        let retired = core::mem::replace(&mut first.transaction, replacement);
        first.success(&mut engine, "SELECT global_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["8"]);
        drop((retired, first, second, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn role_catalog_invalidations_only_recheck_role_dependent_bodies() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("role-plan-invalidation-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut first = ConfiguredTransactionSession::new(&config, &mut budget);
        let mut second =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        first.success(
            &mut engine,
            "CREATE SEQUENCE role_checks; CREATE TYPE role_choice AS ENUM('a'); \
             CREATE FUNCTION role_allowed(value role_choice) RETURNS boolean LANGUAGE plpgsql \
             AS $$BEGIN PERFORM nextval('role_checks'); RETURN true; END$$; \
             CREATE DOMAIN role_checked AS role_choice CHECK(role_allowed(VALUE)); \
             CREATE TABLE role_anchor(id integer); INSERT INTO role_anchor VALUES(1); \
             ALTER TABLE role_anchor ENABLE ROW LEVEL SECURITY; CREATE ROLE role_reader; \
             CREATE FUNCTION role_body() RETURNS role_checked[] LANGUAGE sql \
             SET search_path=public AS 'SELECT ''{a}''::role_checked[] FROM role_anchor LIMIT 1'",
            true,
        );
        let counter = "SELECT last_value FROM role_checks";
        first.success(&mut engine, "SELECT role_body()", true);
        second.success(&mut engine, "SELECT role_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["3"]
        );
        first.success(&mut engine, "BEGIN; ALTER ROLE role_reader LOGIN", true);
        second.success(&mut engine, "SELECT role_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["3"]
        );
        first.success(&mut engine, "SELECT role_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["4"]);
        first.success(&mut engine, "COMMIT; SELECT role_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["5"]);
        second.success(&mut engine, "SELECT role_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["6"]
        );
        first.success(
            &mut engine,
            "ALTER ROLE role_reader LOGIN; SELECT role_body()",
            true,
        );
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["7"]);

        first.success(
            &mut engine,
            "ALTER TABLE role_anchor DISABLE ROW LEVEL SECURITY",
            true,
        );
        first.success(&mut engine, "SELECT role_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["8"]);
        second.success(&mut engine, "SELECT role_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["9"]
        );
        first.success(
            &mut engine,
            "BEGIN; ALTER ROLE role_reader NOLOGIN; SELECT role_body()",
            true,
        );
        second.success(&mut engine, "SELECT role_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["9"]
        );
        first.success(&mut engine, "COMMIT; SELECT role_body()", true);
        second.success(&mut engine, "SELECT role_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["9"]
        );
        drop((first, second, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn schema_owner_noops_preserve_permissions_and_real_changes_require_database_create() {
    let config = test_config("schema-owner-invalidation-noop");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE ROLE schema_reader; CREATE ROLE schema_successor; \
         GRANT schema_successor TO schema_reader; \
         CREATE SCHEMA owner_subject AUTHORIZATION schema_reader; \
         SET ROLE schema_successor; ALTER SCHEMA owner_subject OWNER TO schema_reader; RESET ROLE; \
         SET ROLE schema_reader",
        true,
    );
    let denied = session.execute(
        &mut engine,
        "ALTER SCHEMA owner_subject OWNER TO schema_successor",
        true,
    );
    assert_eq!(error_sqlstates(&denied), ["42501"]);
    assert!(String::from_utf8_lossy(&denied).contains("permission denied for database"));
    session.success(
        &mut engine,
        "RESET ROLE; GRANT CREATE ON DATABASE postgres TO schema_reader; SET ROLE schema_reader; \
         ALTER SCHEMA owner_subject OWNER TO schema_successor; SET ROLE schema_successor; \
         ALTER SCHEMA owner_subject OWNER TO schema_successor; RESET ROLE",
        true,
    );
    drop((session, engine));
    std::fs::remove_dir_all(config.data_dir).unwrap();
}

#[test]
fn prepared_catalog_work_detaches_local_plan_ownership_until_durable_resolution() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("prepared-plan-invalidation-{mode:?}"));
        config.transaction_mode = mode;
        config.max_prepared_transactions = 1;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut first = ConfiguredTransactionSession::new(&config, &mut budget);
        let mut second =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        first.success(&mut engine,
            "CREATE SEQUENCE prepared_checks; CREATE TYPE prepared_choice AS ENUM('a'); \
             CREATE FUNCTION prepared_allowed(value prepared_choice) RETURNS boolean LANGUAGE plpgsql \
             AS $$BEGIN PERFORM nextval('prepared_checks'); RETURN true; END$$; \
             CREATE DOMAIN prepared_checked AS prepared_choice CHECK(prepared_allowed(VALUE)); \
             CREATE TABLE prepared_rows(id integer); \
             CREATE FUNCTION prepared_body() RETURNS prepared_checked[] LANGUAGE sql \
             SET search_path=public AS 'SELECT ''{a}''::prepared_checked[]'", true);
        let counter = "SELECT last_value FROM prepared_checks";
        first.success(&mut engine, "SELECT prepared_body()", true);
        second.success(&mut engine, "SELECT prepared_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["3"]
        );
        first.success(&mut engine,
            "BEGIN; CREATE SCHEMA prepared_committed; SELECT prepared_body(); PREPARE TRANSACTION 'plan-commit'", true);
        first.success(&mut engine, "SELECT prepared_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["5"]);
        second.success(&mut engine, "SELECT prepared_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["5"]
        );
        second.success(&mut engine, "COMMIT PREPARED 'plan-commit'", true);
        second.success(&mut engine, "SELECT prepared_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["6"]
        );
        first.success(&mut engine, "SELECT prepared_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["7"]);

        first.success(&mut engine,
            "BEGIN; CREATE SCHEMA prepared_rolled_back; SELECT prepared_body(); PREPARE TRANSACTION 'plan-rollback'", true);
        first.success(&mut engine, "SELECT prepared_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["9"]);
        second.success(&mut engine, "SELECT prepared_body()", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["9"]
        );
        second.success(&mut engine, "ROLLBACK PREPARED 'plan-rollback'", true);
        second.success(&mut engine, "SELECT prepared_body()", true);
        first.success(&mut engine, "SELECT prepared_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["9"]);
        first.success(
            &mut engine,
            "BEGIN; INSERT INTO prepared_rows VALUES(1); PREPARE TRANSACTION 'plan-data'",
            true,
        );
        first.success(&mut engine, "SELECT prepared_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["9"]);
        second.success(&mut engine, "ROLLBACK PREPARED 'plan-data'", true);
        first.success(&mut engine, "SELECT prepared_body()", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["9"]);
        drop((first, second, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}
