use super::*;

#[test]
fn foreign_owner_changes_check_wrapper_superusers_and_destination_usage() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("foreign-owner-permissions-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(
            &mut engine,
            "CREATE ROLE fdw_old; CREATE ROLE fdw_new; CREATE ROLE fdw_outsider; \
             CREATE ROLE fdw_super SUPERUSER; CREATE ROLE fdw_unreachable; CREATE ROLE fdw_usage; \
             CREATE FOREIGN DATA WRAPPER permission_wrapper; \
             CREATE SERVER permission_server FOREIGN DATA WRAPPER permission_wrapper; \
             ALTER SERVER permission_server OWNER TO fdw_old; \
             GRANT USAGE ON FOREIGN DATA WRAPPER permission_wrapper TO fdw_old; \
             GRANT fdw_new TO fdw_old WITH INHERIT FALSE, SET TRUE",
            true,
        );
        for (identity, command, message, hint) in [
            (
                "fdw_old",
                "ALTER FOREIGN DATA WRAPPER permission_wrapper OWNER TO postgres",
                "permission denied to change owner of foreign-data wrapper \"permission_wrapper\"",
                "Must be superuser to change owner of a foreign-data wrapper.",
            ),
            (
                "postgres",
                "ALTER FOREIGN DATA WRAPPER permission_wrapper OWNER TO fdw_new",
                "permission denied to change owner of foreign-data wrapper \"permission_wrapper\"",
                "The owner of a foreign-data wrapper must be a superuser.",
            ),
            (
                "fdw_outsider",
                "ALTER SERVER permission_server OWNER TO fdw_new",
                "must be owner of foreign server permission_server",
                "",
            ),
            (
                "fdw_old",
                "ALTER SERVER permission_server OWNER TO fdw_unreachable",
                "must be able to SET ROLE \"fdw_unreachable\"",
                "",
            ),
            (
                "fdw_old",
                "ALTER SERVER permission_server OWNER TO fdw_new",
                "permission denied for foreign-data wrapper permission_wrapper",
                "",
            ),
        ] {
            session.success(
                &mut engine,
                &format!("RESET SESSION AUTHORIZATION; SET SESSION AUTHORIZATION {identity}"),
                true,
            );
            let output = session.execute(&mut engine, command, true);
            assert_eq!(error_sqlstates(&output), ["42501"], "{identity}: {command}");
            let wire = String::from_utf8_lossy(&output);
            assert!(wire.contains(&format!("M{message}\0")), "{wire}");
            if !hint.is_empty() {
                assert!(wire.contains(&format!("H{hint}\0")), "{wire}");
            }
        }
        session.success(&mut engine, "RESET SESSION AUTHORIZATION; SET SESSION AUTHORIZATION fdw_outsider; BEGIN; ALTER SERVER permission_server OWNER TO fdw_old", true);
        assert!(session.transaction.ddl().is_empty());
        session.success(&mut engine, "COMMIT; RESET SESSION AUTHORIZATION; ALTER FOREIGN DATA WRAPPER permission_wrapper OWNER TO fdw_super; ALTER ROLE fdw_super NOSUPERUSER", true);
        for identity in ["postgres", "fdw_super"] {
            session.success(
                &mut engine,
                &format!("RESET SESSION AUTHORIZATION; SET SESSION AUTHORIZATION {identity}"),
                true,
            );
            let output = session.execute(
                &mut engine,
                "ALTER FOREIGN DATA WRAPPER permission_wrapper OWNER TO fdw_super",
                true,
            );
            assert_eq!(error_sqlstates(&output), ["42501"]);
            let expected = if identity == "postgres" {
                "The owner of a foreign-data wrapper must be a superuser."
            } else {
                "Must be superuser to change owner of a foreign-data wrapper."
            };
            assert!(String::from_utf8_lossy(&output).contains(&format!("H{expected}\0")));
        }
        session.success(
            &mut engine,
            "RESET SESSION AUTHORIZATION; ALTER ROLE fdw_super SUPERUSER; \
             ALTER FOREIGN DATA WRAPPER permission_wrapper OWNER TO postgres; \
             GRANT USAGE ON FOREIGN DATA WRAPPER permission_wrapper TO fdw_usage; \
             GRANT fdw_usage TO fdw_new WITH INHERIT TRUE, SET FALSE; \
             SET SESSION AUTHORIZATION fdw_old; BEGIN; \
             ALTER SERVER permission_server OWNER TO fdw_new",
            true,
        );
        assert_eq!(data_rows(&session.success(&mut engine, "SELECT pg_get_userbyid(srvowner) FROM pg_foreign_server WHERE srvname='permission_server'", true)), ["fdw_new"]);
        session.success(&mut engine, "ROLLBACK", true);
        assert_eq!(data_rows(&session.success(&mut engine, "SELECT pg_get_userbyid(srvowner) FROM pg_foreign_server WHERE srvname='permission_server'", true)), ["fdw_old"]);
        session.success(
            &mut engine,
            "RESET SESSION AUTHORIZATION; ALTER SERVER permission_server OWNER TO fdw_unreachable",
            true,
        );
        assert_eq!(data_rows(&session.success(&mut engine, "SELECT pg_get_userbyid(srvowner) FROM pg_foreign_server WHERE srvname='permission_server'", true)), ["fdw_unreachable"]);
        drop((session, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn reassign_owned_preserves_target_objects_and_foreign_owners_through_recovery() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("reassign-target-foreign-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(
            &mut engine,
            "CREATE ROLE reassign_source SUPERUSER; CREATE ROLE reassign_target SUPERUSER; \
             CREATE FOREIGN DATA WRAPPER reassign_wrapper; \
             ALTER FOREIGN DATA WRAPPER reassign_wrapper OWNER TO reassign_source; \
             CREATE SERVER reassign_server FOREIGN DATA WRAPPER reassign_wrapper; \
             ALTER SERVER reassign_server OWNER TO reassign_source; \
             CREATE COLLATION reassign_collation (PROVIDER=libc, LOCALE='C'); \
             ALTER COLLATION reassign_collation OWNER TO reassign_source",
            true,
        );
        let owners = "SELECT pg_get_userbyid(fdwowner) FROM pg_foreign_data_wrapper WHERE fdwname='reassign_wrapper'; \
                      SELECT pg_get_userbyid(srvowner) FROM pg_foreign_server WHERE srvname='reassign_server'; \
                      SELECT pg_get_userbyid(collowner) FROM pg_collation WHERE collname='reassign_collation'";
        session.success(&mut engine, "BEGIN", true);
        session.success(
            &mut engine,
            "REASSIGN OWNED BY reassign_source TO reassign_source; REASSIGN OWNED BY reassign_source TO reassign_source",
            true,
        );
        assert!(session.transaction.ddl().is_empty());
        session.success(
            &mut engine,
            "REASSIGN OWNED BY reassign_source,reassign_target TO reassign_target",
            true,
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, owners, true)),
            ["reassign_target"; 3]
        );
        session.success(&mut engine, "ROLLBACK", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, owners, true)),
            ["reassign_source"; 3]
        );
        session.success(
            &mut engine,
            "REASSIGN OWNED BY reassign_source,reassign_target TO reassign_target",
            true,
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, owners, true)),
            ["reassign_target"; 3]
        );
        drop((session, engine));
        let mut reopened_budget = test_engine_budget(&config, 1 << 29);
        let mut reopened = Engine::new(&config, &mut reopened_budget).unwrap();
        let mut reopened_session = ConfiguredTransactionSession::new(&config, &mut reopened_budget);
        assert_eq!(
            data_rows(&reopened_session.success(&mut reopened, owners, true)),
            ["reassign_target"; 3]
        );
        reopened_session.success(&mut reopened, "BEGIN", true);
        reopened_session.success(
            &mut reopened,
            "REASSIGN OWNED BY reassign_source,reassign_target TO reassign_target",
            true,
        );
        assert!(reopened_session.transaction.ddl().is_empty());
        reopened_session.success(&mut reopened, "ROLLBACK", true);
        drop((reopened_session, reopened));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn owner_commands_report_events_without_repeating_catalog_mutations() {
    let config = test_config("owner-noop-event");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE ROLE owner_event_role; CREATE ROLE owner_event_wrapper_role SUPERUSER; \
         GRANT CREATE ON SCHEMA public TO owner_event_role; \
         CREATE TABLE owner_event_target(value integer); \
         CREATE FOREIGN DATA WRAPPER owner_event_wrapper; \
         CREATE SERVER owner_event_server FOREIGN DATA WRAPPER owner_event_wrapper; \
         CREATE TABLE owner_event_log(tag text,kind text,identity text); \
         CREATE FUNCTION owner_event_capture() RETURNS event_trigger LANGUAGE plpgsql AS \
         $$BEGIN INSERT INTO owner_event_log SELECT command_tag,object_type,object_identity \
         FROM pg_event_trigger_ddl_commands(); RETURN; END$$; \
         CREATE EVENT TRIGGER owner_event ON ddl_command_end WHEN TAG IN ('ALTER TABLE','ALTER SERVER','ALTER FOREIGN DATA WRAPPER') \
         EXECUTE FUNCTION owner_event_capture()",
        true,
    );
    session.success(&mut engine, "BEGIN", true);
    assert!(session.transaction.ddl().is_empty());
    session.success(
        &mut engine,
        "ALTER TABLE owner_event_target OWNER TO postgres",
        true,
    );
    assert!(session.transaction.ddl().is_empty());
    session.success(
        &mut engine,
        "ALTER TABLE owner_event_target OWNER TO owner_event_role",
        true,
    );
    let changed = session.transaction.ddl().len();
    assert!(changed > 0);
    session.success(
        &mut engine,
        "ALTER TABLE owner_event_target OWNER TO owner_event_role",
        true,
    );
    assert_eq!(session.transaction.ddl().len(), changed);
    for statement in [
        "ALTER SERVER owner_event_server OWNER TO postgres",
        "ALTER SERVER owner_event_server OWNER TO owner_event_role",
        "ALTER SERVER owner_event_server OWNER TO owner_event_role",
    ] {
        session.success(&mut engine, statement, true);
    }
    for statement in [
        "ALTER FOREIGN DATA WRAPPER owner_event_wrapper OWNER TO postgres",
        "ALTER FOREIGN DATA WRAPPER owner_event_wrapper OWNER TO owner_event_wrapper_role",
        "ALTER FOREIGN DATA WRAPPER owner_event_wrapper OWNER TO owner_event_wrapper_role",
    ] {
        session.success(&mut engine, statement, true);
    }
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT tag,kind,identity FROM owner_event_log ORDER BY tag",
            true
        )),
        [
            "ALTER FOREIGN DATA WRAPPER|foreign-data wrapper|owner_event_wrapper",
            "ALTER FOREIGN DATA WRAPPER|foreign-data wrapper|owner_event_wrapper",
            "ALTER FOREIGN DATA WRAPPER|foreign-data wrapper|owner_event_wrapper",
            "ALTER SERVER|server|owner_event_server",
            "ALTER SERVER|server|owner_event_server",
            "ALTER SERVER|server|owner_event_server",
            "ALTER TABLE|table|public.owner_event_target",
            "ALTER TABLE|table|public.owner_event_target",
            "ALTER TABLE|table|public.owner_event_target",
        ]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine,
            "SELECT type,identity FROM pg_identify_object('pg_foreign_data_wrapper'::regclass,(SELECT oid FROM pg_foreign_data_wrapper WHERE fdwname='owner_event_wrapper'),0); \
             SELECT type,identity FROM pg_identify_object('pg_foreign_server'::regclass,(SELECT oid FROM pg_foreign_server WHERE srvname='owner_event_server'),0); \
             SELECT classid='pg_foreign_data_wrapper'::regclass FROM pg_get_object_address('foreign-data wrapper',ARRAY['owner_event_wrapper'],ARRAY[]::text[]); \
             SELECT classid='pg_foreign_server'::regclass FROM pg_get_object_address('server',ARRAY['owner_event_server'],ARRAY[]::text[])", true)),
        ["foreign-data wrapper|owner_event_wrapper", "server|owner_event_server", "t", "t"]
    );
    let owners = "SELECT pg_get_userbyid(fdwowner) FROM pg_foreign_data_wrapper WHERE fdwname='owner_event_wrapper'; \
                  SELECT pg_get_userbyid(srvowner) FROM pg_foreign_server WHERE srvname='owner_event_server'";
    assert_eq!(
        data_rows(&session.success(&mut engine, owners, true)),
        ["owner_event_wrapper_role", "owner_event_role"]
    );
    session.success(&mut engine, "ROLLBACK", true);
    assert_eq!(
        data_rows(&session.success(&mut engine, owners, true)),
        ["postgres", "postgres"]
    );
    session.success(&mut engine, "SET ROLE owner_event_role", true);
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "ALTER FOREIGN DATA WRAPPER owner_event_wrapper OWNER TO owner_event_wrapper_role",
            true
        )),
        [sqlstate::INSUFFICIENT_PRIVILEGE]
    );
    session.success(&mut engine, "RESET ROLE", true);
}

#[test]
fn ownership_uses_inherited_privileges_and_accepts_same_owner_commands() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("owner-inheritance-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(
            &mut engine,
            "CREATE ROLE owner_group; CREATE ROLE set_only; CREATE ROLE inherited_owner; \
             GRANT owner_group TO set_only WITH INHERIT FALSE, SET TRUE; \
             GRANT owner_group TO inherited_owner WITH INHERIT TRUE, SET FALSE; \
             CREATE FOREIGN DATA WRAPPER owner_wrapper; GRANT USAGE ON FOREIGN DATA WRAPPER owner_wrapper TO owner_group; \
             GRANT CREATE ON SCHEMA public TO owner_group; SET ROLE owner_group; \
             CREATE TABLE group_table(id integer); \
             CREATE FUNCTION group_function() RETURNS integer LANGUAGE sql AS 'SELECT 1'; \
             CREATE TYPE group_choice AS ENUM('a'); CREATE SERVER group_server FOREIGN DATA WRAPPER owner_wrapper; \
             CREATE COLLATION group_collation (PROVIDER=libc, LOCALE='C'); RESET ROLE; \
             SET SESSION AUTHORIZATION set_only; ALTER TYPE group_choice OWNER TO owner_group; \
             ALTER SERVER group_server OWNER TO owner_group",
            true,
        );
        for statement in [
            "ALTER TABLE group_table OWNER TO owner_group",
            "ALTER TABLE group_table ADD COLUMN denied integer",
            "ALTER FUNCTION group_function() STABLE",
            "ALTER TYPE group_choice ADD VALUE 'denied'",
            "ALTER SERVER group_server VERSION 'denied'",
            "ALTER COLLATION group_collation RENAME TO denied_collation",
            "ALTER DEFAULT PRIVILEGES FOR ROLE owner_group GRANT SELECT ON TABLES TO set_only",
            "DROP OWNED BY owner_group",
            "REASSIGN OWNED BY owner_group TO set_only",
        ] {
            assert_eq!(
                error_sqlstates(&session.execute(&mut engine, statement, true)),
                ["42501"]
            );
        }
        session.success(
            &mut engine,
            "RESET SESSION AUTHORIZATION; SET SESSION AUTHORIZATION inherited_owner; \
             ALTER TABLE group_table OWNER TO owner_group; ALTER TABLE group_table ADD COLUMN allowed integer; \
             ALTER FUNCTION group_function() STABLE; \
             ALTER TYPE group_choice ADD VALUE 'allowed'; ALTER SERVER group_server VERSION 'allowed'; \
             ALTER COLLATION group_collation RENAME TO allowed_collation; \
             ALTER DEFAULT PRIVILEGES FOR ROLE owner_group GRANT SELECT ON TABLES TO set_only; \
             REASSIGN OWNED BY owner_group TO owner_group",
            true,
        );
        session.success(&mut engine, "BEGIN", true);
        session.success(
            &mut engine,
            "REASSIGN OWNED BY owner_group TO owner_group; REASSIGN OWNED BY owner_group TO owner_group",
            true,
        );
        assert!(session.transaction.ddl().is_empty());
        session.success(&mut engine, "COMMIT", true);
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, "SET ROLE owner_group", true)),
            ["42501"]
        );
        session.success(
            &mut engine,
            "DROP OWNED BY owner_group; RESET SESSION AUTHORIZATION",
            true,
        );
        drop((session, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}
