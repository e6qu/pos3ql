use super::*;

#[test]
fn runtime_checked_inputs_follow_selected_definitions_and_replace_changed_paths() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("runtime-input-validity-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE SCHEMA path_alternative; CREATE SEQUENCE validity_checks; \
             CREATE TYPE validity_choice AS ENUM('a'); \
             CREATE FUNCTION validity_allowed(value validity_choice) RETURNS boolean LANGUAGE plpgsql \
             AS $$BEGIN PERFORM nextval('validity_checks'); RETURN true; END$$; \
             CREATE DOMAIN validity_checked AS validity_choice CHECK(validity_allowed(VALUE)); \
             CREATE TABLE validity_anchor(id integer); INSERT INTO validity_anchor VALUES(1); \
             CREATE TABLE validity_unrelated(id integer); \
             CREATE FUNCTION validity_body() RETURNS validity_checked[] LANGUAGE sql SECURITY DEFINER \
             AS 'SELECT ''{a}''::public.validity_checked[] FROM public.validity_anchor LIMIT 1'", true);
        let counter = "SELECT last_value FROM public.validity_checks";
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["1"]
        );
        for expected in ["2", "2"] {
            session.success(&mut engine, "SELECT public.validity_body()", true);
            assert_eq!(
                data_rows(&session.success(&mut engine, counter, true)),
                [expected]
            );
        }
        session.success(&mut engine, "INSERT INTO validity_anchor VALUES(2); ALTER TABLE validity_unrelated ADD COLUMN ignored text", true);
        session.success(&mut engine, "SELECT public.validity_body()", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["2"]
        );
        session.success(
            &mut engine,
            "ALTER TABLE validity_anchor ADD COLUMN added text",
            true,
        );
        session.success(&mut engine, "SELECT public.validity_body()", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["3"]
        );
        for (path, expected) in [("public,path_alternative", "4"), ("public", "5")] {
            session.success(&mut engine, &format!("SET search_path={path}"), true);
            session.success(&mut engine, "SELECT public.validity_body()", true);
            assert_eq!(
                data_rows(&session.success(&mut engine, counter, true)),
                [expected]
            );
        }
        session.success(&mut engine,
            "CREATE FUNCTION validity_outer() RETURNS integer LANGUAGE sql \
             SET search_path=public,path_alternative AS 'SELECT cardinality(public.validity_body())'", true);
        session.success(&mut engine, "SELECT validity_outer()", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["6"]
        );
        session.success(&mut engine, "SELECT validity_body()", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["7"]
        );
        session.success(&mut engine,
            "CREATE VIEW validity_view AS SELECT id FROM validity_anchor; \
             CREATE FUNCTION validity_view_body() RETURNS validity_checked[] LANGUAGE sql SECURITY DEFINER \
             AS 'SELECT ''{a}''::public.validity_checked[] FROM public.validity_view LIMIT 1'", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["8"]
        );
        session.success(&mut engine, "SELECT validity_view_body()", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["9"]
        );
        session.success(
            &mut engine,
            "ALTER TABLE validity_anchor ADD COLUMN through_view text",
            true,
        );
        session.success(&mut engine, "SELECT validity_view_body()", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["10"]
        );
        drop((session, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn selected_operator_and_cast_functions_participate_in_runtime_plan_validity() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("support-function-validity-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE FUNCTION selected_operator(integer,integer) RETURNS integer LANGUAGE sql AS $$SELECT $1+$2$$; \
             CREATE OPERATOR public.##(LEFTARG=integer,RIGHTARG=integer,FUNCTION=selected_operator); \
             CREATE FUNCTION selected_cast(integer,integer,boolean) RETURNS varchar LANGUAGE sql AS $$SELECT 'x'::varchar$$; \
             CREATE CAST(integer AS varchar) WITH FUNCTION selected_cast(integer,integer,boolean) AS IMPLICIT; \
             CREATE FUNCTION consume_varchar(varchar) RETURNS integer LANGUAGE sql AS $$SELECT 1$$; \
             CREATE FUNCTION unselected_support() RETURNS integer LANGUAGE sql AS $$SELECT 1$$; \
             CREATE TABLE selected_destination(value varchar(5),items varchar(5)[])", true);
        let arena =
            Arena::new(&mut budget, "support function dependency metadata", 2 << 20).unwrap();
        for (statement, support, replacement) in [
            (
                "SELECT 1 OPERATOR(public.##) 2",
                "selected_operator",
                "CREATE OR REPLACE FUNCTION selected_operator(integer,integer) RETURNS integer LANGUAGE sql AS $$SELECT $1-$2$$",
            ),
            (
                "SELECT 7::varchar(5)",
                "selected_cast",
                "CREATE OR REPLACE FUNCTION selected_cast(integer,integer,boolean) RETURNS varchar LANGUAGE sql AS $$SELECT 'a'::varchar$$",
            ),
            (
                "SELECT ARRAY[7,8]::varchar(5)[]",
                "selected_cast",
                "CREATE OR REPLACE FUNCTION selected_cast(integer,integer,boolean) RETURNS varchar LANGUAGE sql AS $$SELECT 'b'::varchar$$",
            ),
            (
                "SELECT consume_varchar(7)",
                "selected_cast",
                "CREATE OR REPLACE FUNCTION selected_cast(integer,integer,boolean) RETURNS varchar LANGUAGE sql AS $$SELECT 'c'::varchar$$",
            ),
            (
                "INSERT INTO selected_destination VALUES(7,ARRAY[7,8])",
                "selected_cast",
                "CREATE OR REPLACE FUNCTION selected_cast(integer,integer,boolean) RETURNS varchar LANGUAGE sql AS $$SELECT 'd'::varchar$$",
            ),
        ] {
            let validity = crate::mem::guard::forbid_alloc(|| {
                let statement = parser::parse_stored_statement(statement, &arena).unwrap();
                let mut dependencies = engine
                    .storage
                    .empty_stored_query_dependencies(&arena)
                    .unwrap();
                query::collect_runtime_statement(
                    statement,
                    &engine.storage,
                    session.transaction.txid,
                    &mut dependencies,
                    &arena,
                    None,
                )
                .unwrap();
                assert!(
                    dependencies.entries().iter().any(|entry| {
                        entry.class == crate::storage::DependencyClass::Routine
                            && entry.name.as_str() == support
                    }),
                    "{statement:?}: {support}"
                );
                engine
                    .storage
                    .capture_runtime_input_validity(&dependencies, &session.transaction, None)
                    .unwrap()
            });
            assert!(
                validity
                    .matches(&engine.storage, session.transaction.txid)
                    .unwrap()
            );
            session.success(&mut engine,
                "CREATE OR REPLACE FUNCTION unselected_support() RETURNS integer LANGUAGE sql AS $$SELECT 2$$", true);
            assert!(
                validity
                    .matches(&engine.storage, session.transaction.txid)
                    .unwrap(),
                "unrelated replacement: {statement}"
            );
            session.success(&mut engine, replacement, true);
            assert!(
                !validity
                    .matches(&engine.storage, session.transaction.txid)
                    .unwrap(),
                "selected replacement: {statement}"
            );
        }
    }
}
