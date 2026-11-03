use super::*;

#[test]
fn unchanged_table_ownership_reports_one_event_without_mutating_the_catalog() {
    let config = test_config("owner-noop-event");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE ROLE owner_event_role; GRANT CREATE ON SCHEMA public TO owner_event_role; \
         CREATE TABLE owner_event_target(value integer); \
         CREATE TABLE owner_event_log(tag text,kind text,identity text); \
         CREATE FUNCTION owner_event_capture() RETURNS event_trigger LANGUAGE plpgsql AS \
         $$BEGIN INSERT INTO owner_event_log SELECT command_tag,object_type,object_identity \
         FROM pg_event_trigger_ddl_commands(); RETURN; END$$; \
         CREATE EVENT TRIGGER owner_event ON ddl_command_end WHEN TAG IN ('ALTER TABLE') \
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
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT tag,kind,identity FROM owner_event_log",
            true
        )),
        ["ALTER TABLE|table|public.owner_event_target"; 3]
    );
    session.success(&mut engine, "ROLLBACK", true);
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
