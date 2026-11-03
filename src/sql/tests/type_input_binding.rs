use super::*;

// PostgreSQL 18.6 PREPARE reference SQL SHA256:
// daf5154b24ffeb86c7deae60b70570fa4bef142d2865b12b9c93d650e32c77a5.
#[test]
fn implicit_domain_array_inputs_are_checked_at_prepare_and_reused() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("implicit-domain-inputs-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE SEQUENCE implicit_checks; CREATE TABLE implicit_audit(value text); \
             CREATE TYPE implicit_choice AS ENUM('a','b'); \
             CREATE FUNCTION implicit_allowed(value implicit_choice) RETURNS boolean LANGUAGE plpgsql AS $$BEGIN \
               PERFORM nextval('implicit_checks'); INSERT INTO implicit_audit VALUES(value::text); RETURN value='a'; END$$; \
             CREATE DOMAIN implicit_checked AS implicit_choice CHECK(implicit_allowed(VALUE)); \
             CREATE TABLE implicit_values(id integer PRIMARY KEY,value implicit_checked[]); \
             CREATE FUNCTION take_checked(value implicit_checked[]) RETURNS integer LANGUAGE sql AS 'SELECT cardinality(value)'", true);
        let counter = "SELECT currval('implicit_checks'),count(*) FROM implicit_audit";
        for (prepare, execute, expected) in [
            (
                "PREPARE implicit_insert AS INSERT INTO implicit_values VALUES(1,'{a}')",
                "EXECUTE implicit_insert",
                "1|1",
            ),
            (
                "PREPARE implicit_call AS SELECT take_checked('{a}')",
                "EXECUTE implicit_call; EXECUTE implicit_call",
                "2|2",
            ),
            (
                "PREPARE implicit_update AS UPDATE implicit_values SET value='{a}' WHERE id=1",
                "EXECUTE implicit_update",
                "3|3",
            ),
            (
                "PREPARE explicit_insert AS INSERT INTO implicit_values VALUES(2,'{a}'::implicit_checked[])",
                "EXECUTE explicit_insert",
                "4|4",
            ),
        ] {
            session.success(&mut engine, prepare, true);
            assert_eq!(
                data_rows(&session.success(&mut engine, counter, true)),
                [expected]
            );
            session.success(&mut engine, execute, true);
            assert_eq!(
                data_rows(&session.success(&mut engine, counter, true)),
                [expected]
            );
        }
        session.success(
            &mut engine,
            "INSERT INTO implicit_values SELECT 3,'{a}'",
            true,
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["5|5"]
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, "SELECT 7 WHERE take_checked('{a}')=1", true)),
            ["7"]
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["6|6"]
        );
        for (statement, expected) in [
            (
                "PREPARE bad_implicit_insert AS INSERT INTO implicit_values VALUES(4,'{b}')",
                "7|6",
            ),
            (
                "PREPARE bad_implicit_call AS SELECT take_checked('{b}')",
                "8|6",
            ),
            (
                "PREPARE bad_implicit_update AS UPDATE implicit_values SET value='{b}'",
                "9|6",
            ),
        ] {
            let error = session.execute(&mut engine, statement, true);
            assert!(
                error.windows(5).any(|bytes| bytes == b"23514"),
                "{}",
                String::from_utf8_lossy(&error)
            );
            assert_eq!(
                data_rows(&session.success(&mut engine, counter, true)),
                [expected]
            );
        }
        session.success(&mut engine,
            "BEGIN; PREPARE retained_implicit_input AS SELECT take_checked('{a}'); ROLLBACK; EXECUTE retained_implicit_input", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["10|6"]
        );
    }
}

// PostgreSQL 18.6 input-order SQL SHA256:
// de9332b4b78c97aa4ed4e18c5d78717ca526d7183df3eb532afbcc7dbfeee16a,
// 78f032b6ce047aa906c1e33276c0af00ea519cd360ad975d7fd6fdae7d8bc512.
#[test]
fn domain_input_discovery_preserves_expression_and_row_coercion_order() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("domain-input-order-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE SEQUENCE ordered_inputs; CREATE TABLE input_order(position bigint,label text); \
             CREATE TYPE ordered_choice AS ENUM('a','b'); \
             CREATE FUNCTION record_ordered_input(value ordered_choice) RETURNS boolean LANGUAGE plpgsql AS $$BEGIN \
               INSERT INTO input_order VALUES(nextval('ordered_inputs'),value::text); RETURN true; END$$; \
             CREATE DOMAIN ordered_checked AS ordered_choice CHECK(record_ordered_input(VALUE)); \
             CREATE FUNCTION take_ordered(value ordered_checked[]) RETURNS integer LANGUAGE sql AS 'SELECT cardinality(value)'; \
             CREATE FUNCTION take_two_ordered(first_value ordered_checked[],second_value ordered_checked[]) RETURNS integer LANGUAGE sql AS 'SELECT cardinality(first_value)+cardinality(second_value)'; \
             CREATE TABLE ordered_values(first_value ordered_checked[],second_value ordered_checked[])", true);
        for (query, expected) in [
            ("SELECT take_ordered('{a}'),'{b}'::ordered_checked[]", "a,b"),
            (
                "SELECT take_two_ordered(second_value=>'{b}',first_value=>'{a}')",
                "b,a",
            ),
            (
                "SELECT take_two_ordered('{a}','{b}'::ordered_checked[])",
                "b,a",
            ),
            (
                "INSERT INTO ordered_values VALUES('{a}','{b}'::ordered_checked[])",
                "b,a",
            ),
            (
                "UPDATE ordered_values SET first_value='{a}',second_value='{b}'::ordered_checked[]",
                "b,a",
            ),
            (
                "INSERT INTO ordered_values SELECT '{a}','{b}'::ordered_checked[]",
                "b,a",
            ),
            (
                "TRUNCATE ordered_values; INSERT INTO ordered_values VALUES('{a}','{b}'::ordered_checked[]),('{b}','{a}'::ordered_checked[])",
                "b,a,a,b",
            ),
        ] {
            session.success(&mut engine, query, true);
            assert_eq!(
                data_rows(&session.success(
                    &mut engine,
                    "SELECT string_agg(label,',' ORDER BY position) FROM input_order",
                    true
                )),
                [expected],
                "{query}"
            );
            session.success(&mut engine, "TRUNCATE input_order", true);
        }
    }
}

#[test]
fn routine_input_binding_uses_its_owner_and_settings_then_restores_the_caller() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("routine-input-context-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let owner = data_rows(&session.success(&mut engine, "SELECT current_user", true)).remove(0);
        session.success(&mut engine,
            "CREATE ROLE input_context_caller; CREATE SCHEMA input_context; \
             CREATE TABLE input_context.audit(role_name text,schema_name text,application text); \
             CREATE TYPE input_context.choice AS ENUM('a'); \
             CREATE FUNCTION input_context.record(value input_context.choice) RETURNS boolean LANGUAGE plpgsql AS $$BEGIN \
               INSERT INTO input_context.audit VALUES(current_user,current_schema(),current_setting('application_name')); RETURN true; END$$; \
             CREATE DOMAIN input_context.checked AS input_context.choice CHECK(input_context.record(VALUE)); \
             CREATE FUNCTION public.input_scope_scalar() RETURNS integer LANGUAGE sql SECURITY DEFINER \
               SET search_path TO input_context,pg_catalog SET application_name TO 'routine-input' \
               AS 'SELECT cardinality(''{a}''::checked[])'; \
             CREATE FUNCTION public.input_scope_rows() RETURNS SETOF integer LANGUAGE sql SECURITY DEFINER \
               SET search_path TO input_context,pg_catalog SET application_name TO 'routine-input' \
               AS 'SELECT cardinality(''{a}''::checked[])'; \
             REVOKE ALL ON SCHEMA input_context FROM PUBLIC; \
             SET search_path TO public; SET application_name TO 'caller-input'; SET ROLE input_context_caller", true);
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT public.input_scope_scalar(),current_user,current_schema(),current_setting('application_name')", true)),
            ["1|input_context_caller|public|caller-input"]);
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT value,current_user,current_schema(),current_setting('application_name') FROM public.input_scope_rows() AS value", true)),
            ["1|input_context_caller|public|caller-input"]);
        session.success(&mut engine, "RESET ROLE", true);
        let audit = data_rows(&session.success(
            &mut engine,
            "SELECT role_name,schema_name,application FROM input_context.audit",
            true,
        ));
        let expected = format!("{owner}|input_context|routine-input");
        assert_eq!(audit, [expected.clone(), expected]);
    }
}
