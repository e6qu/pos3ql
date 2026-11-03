use super::*;

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
             GRANT CREATE ON SCHEMA public TO owner_group; SET ROLE owner_group; \
             CREATE TABLE group_table(id integer); \
             CREATE FUNCTION group_function() RETURNS integer LANGUAGE sql AS 'SELECT 1'; \
             CREATE TYPE group_choice AS ENUM('a'); RESET ROLE; \
             SET SESSION AUTHORIZATION set_only; ALTER TABLE group_table OWNER TO owner_group",
            true,
        );
        for statement in [
            "ALTER TABLE group_table ADD COLUMN denied integer",
            "ALTER FUNCTION group_function() STABLE",
            "ALTER TYPE group_choice ADD VALUE 'denied'",
        ] {
            assert_eq!(
                error_sqlstates(&session.execute(&mut engine, statement, true)),
                ["42501"]
            );
        }
        session.success(
            &mut engine,
            "RESET SESSION AUTHORIZATION; SET SESSION AUTHORIZATION inherited_owner; \
             ALTER TABLE group_table ADD COLUMN allowed integer; \
             ALTER FUNCTION group_function() STABLE; \
             ALTER TYPE group_choice ADD VALUE 'allowed'",
            true,
        );
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, "SET ROLE owner_group", true)),
            ["42501"]
        );
        session.success(&mut engine, "RESET SESSION AUTHORIZATION", true);
    }
}
