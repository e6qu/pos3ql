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
fn checks_that_create_namespaces_finish_one_input_plan_for_repeated_calls() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("checked-namespace-reentrant-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE SEQUENCE reentrant_checks; CREATE TYPE reentrant_choice AS ENUM('a'); \
             CREATE FUNCTION reentrant_allowed(value reentrant_choice) RETURNS boolean LANGUAGE plpgsql AS $$ \
             BEGIN PERFORM nextval('reentrant_checks'); CREATE SCHEMA IF NOT EXISTS reentrant_created; RETURN true; END$$; \
             CREATE DOMAIN reentrant_checked AS reentrant_choice CHECK(reentrant_allowed(VALUE)); \
             SET check_function_bodies=off; \
             CREATE FUNCTION reentrant_body() RETURNS reentrant_checked[] LANGUAGE sql SET search_path=public \
             AS 'SELECT ''{a}''::reentrant_checked[]'; SET check_function_bodies=on", true);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT reentrant_body(),reentrant_body()",
                true
            )),
            ["{a}|{a}"]
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT last_value FROM reentrant_checks",
                true
            )),
            ["1"]
        );
        session.success(&mut engine, "DROP SCHEMA reentrant_created", true);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT reentrant_body(),reentrant_body()",
                true
            )),
            ["{a}|{a}"]
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT last_value FROM reentrant_checks",
                true
            )),
            ["2"]
        );
        drop((session, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn procedural_sites_recheck_only_reached_statements_with_changed_dependencies() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("procedural-selected-plan-invalidation-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut first = ConfiguredTransactionSession::new(&config, &mut budget);
        let mut second =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        first.success(&mut engine,
            "CREATE SEQUENCE pl_valid_checks; CREATE TYPE pl_valid_choice AS ENUM('a','b'); \
             CREATE FUNCTION pl_valid_allowed(value pl_valid_choice) RETURNS boolean LANGUAGE plpgsql \
             AS $$BEGIN PERFORM nextval('pl_valid_checks'); RETURN true; END$$; \
             CREATE DOMAIN pl_valid_checked AS pl_valid_choice CHECK(pl_valid_allowed(VALUE)); \
             CREATE TABLE pl_valid_left(id integer); CREATE TABLE pl_valid_right(id integer); \
             INSERT INTO pl_valid_left VALUES(1); INSERT INTO pl_valid_right VALUES(1); \
             CREATE FUNCTION pl_valid_body(take_left boolean) RETURNS pl_valid_checked[] LANGUAGE plpgsql AS $$ \
             BEGIN IF take_left THEN RETURN (SELECT '{a}'::pl_valid_checked[] FROM pl_valid_left LIMIT 1); \
             ELSE RETURN (SELECT '{b}'::pl_valid_checked[] FROM pl_valid_right LIMIT 1); END IF; END$$; \
             CREATE FUNCTION pl_valid_static(take_left boolean) RETURNS pl_valid_checked[] LANGUAGE plpgsql AS $$ \
             DECLARE answer pl_valid_checked[]; BEGIN IF take_left THEN \
             SELECT '{a}'::pl_valid_checked[] INTO answer FROM pl_valid_left WHERE take_left LIMIT 1; ELSE \
             SELECT '{b}'::pl_valid_checked[] INTO answer FROM pl_valid_right WHERE NOT take_left LIMIT 1; END IF; \
             RETURN answer; END$$", true);
        let counter = "SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM pl_valid_checks";
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["0"]);
        first.success(
            &mut engine,
            "SELECT pl_valid_body(true); SELECT pl_valid_body(true)",
            true,
        );
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["1"]);
        first.success(
            &mut engine,
            "SELECT pl_valid_body(false); SELECT pl_valid_body(true)",
            true,
        );
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["2"]);
        first.success(
            &mut engine,
            "ALTER TABLE pl_valid_left ADD COLUMN note text",
            true,
        );
        first.success(&mut engine, "SELECT pl_valid_body(false)", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["2"]);
        first.success(
            &mut engine,
            "SELECT pl_valid_body(true); SELECT pl_valid_body(true)",
            true,
        );
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["3"]);

        second.success(&mut engine, "SELECT pl_valid_body(true)", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["4"]
        );
        second.success(
            &mut engine,
            "BEGIN; ALTER TABLE pl_valid_left ADD COLUMN pending text; SELECT pl_valid_body(true)",
            true,
        );
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["5"]
        );
        first.success(&mut engine, "SELECT pl_valid_body(true)", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["5"]);
        second.success(&mut engine, "ROLLBACK; SELECT pl_valid_body(true)", true);
        assert_eq!(
            data_rows(&second.success(&mut engine, counter, true)),
            ["6"]
        );
        first.success(&mut engine, "SELECT pl_valid_body(true)", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["6"]);
        first.success(
            &mut engine,
            "SELECT pl_valid_static(true); SELECT pl_valid_static(false)",
            true,
        );
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["8"]);
        first.success(
            &mut engine,
            "ALTER TABLE pl_valid_left ADD COLUMN static_note text",
            true,
        );
        first.success(&mut engine, "SELECT pl_valid_static(false)", true);
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["8"]);
        first.success(
            &mut engine,
            "SELECT pl_valid_static(true); SELECT pl_valid_static(true)",
            true,
        );
        assert_eq!(data_rows(&first.success(&mut engine, counter, true)), ["9"]);
        drop((first, second, engine));
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
