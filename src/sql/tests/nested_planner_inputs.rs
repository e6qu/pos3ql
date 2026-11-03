use super::*;

#[test]
fn accepted_inline_bodies_use_normalized_constants_without_guessing_unknown_values() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("nested-planner-inputs-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE SEQUENCE nested_checks; CREATE TYPE nested_choice AS ENUM('a'); \
             CREATE DOMAIN nested_checked AS nested_choice CHECK(nextval('nested_checks')>0); \
             CREATE FUNCTION nested_inner() RETURNS integer LANGUAGE sql AS \
               $$SELECT cardinality('{a}'::nested_checked[])$$; \
             CREATE FUNCTION nested_outer(flag integer DEFAULT 1) RETURNS integer LANGUAGE sql AS \
               $$SELECT CASE WHEN flag=0 THEN nested_inner() ELSE 17 END$$; \
             CREATE FUNCTION nested_pair(flag integer,unused integer) RETURNS integer LANGUAGE sql AS \
               $$SELECT CASE WHEN flag=0 THEN nested_inner() ELSE 17 END$$; \
             CREATE FUNCTION nested_coalesce(value integer) RETURNS integer LANGUAGE sql AS \
               $$SELECT coalesce(value,nested_inner())$$; \
             CREATE FUNCTION nested_negative(flag integer DEFAULT -1) RETURNS integer LANGUAGE sql AS \
               $$SELECT CASE WHEN flag<0 THEN 17 ELSE nested_inner() END$$; \
             CREATE FUNCTION nested_variadic(VARIADIC items integer[]) RETURNS integer LANGUAGE sql AS \
               $$SELECT CASE WHEN $1[1] IS NULL THEN 17 ELSE nested_inner() END$$; \
             CREATE FUNCTION nested_direct(flag integer) RETURNS integer LANGUAGE sql AS \
               $$SELECT CASE WHEN flag=0 THEN cardinality('{a}'::nested_checked[]) ELSE 17 END$$", true);
        for (query, rows, checks) in [
            ("SELECT nested_outer(1)", &["17"][..], "0"),
            ("SELECT nested_outer()", &["17"][..], "0"),
            ("SELECT nested_outer(flag=>1)", &["17"][..], "0"),
            ("SELECT nested_outer(1::smallint)", &["17"][..], "0"),
            ("SELECT nested_outer(NULL)", &["17"][..], "0"),
            ("SELECT nested_outer(0)", &["1"][..], "1"),
            ("SELECT nested_outer(flag=>0)", &["1"][..], "1"),
            (
                "SELECT nested_pair(1,value) FROM (VALUES(NULL::integer),(0)) source(value)",
                &["17", "17"][..],
                "0",
            ),
            (
                "SELECT nested_outer(value) FROM (VALUES(1),(0)) source(value)",
                &["17", "1"][..],
                "1",
            ),
            ("SELECT nested_coalesce(17)", &["17"][..], "0"),
            ("SELECT nested_coalesce(NULL)", &["1"][..], "1"),
            ("SELECT nested_negative()", &["17"][..], "0"),
            ("SELECT nested_variadic(1,2)", &["1"][..], "1"),
            (
                "SELECT nested_variadic(VARIADIC ARRAY[]::integer[])",
                &["17"][..],
                "0",
            ),
            (
                "SELECT nested_variadic(VARIADIC ARRAY[NULL]::integer[])",
                &["17"][..],
                "0",
            ),
            ("SELECT nested_direct(1)", &["17"][..], "1"),
        ] {
            session.success(
                &mut engine,
                "ALTER SEQUENCE nested_checks RESTART WITH 1",
                true,
            );
            assert_eq!(
                data_rows(&session.success(&mut engine, query, true)),
                rows,
                "{query}"
            );
            assert_eq!(
                data_rows(&session.success(
                    &mut engine,
                    "SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM nested_checks",
                    true
                )),
                [checks],
                "{query}"
            );
        }
        session.success(&mut engine,
            "CREATE FUNCTION nested_varchar(integer,integer,boolean) RETURNS varchar LANGUAGE sql AS \
               $$SELECT (CASE WHEN $2=9 AND NOT $3 THEN format('%s',$1) ELSE format('%s',nested_inner()) END)::varchar$$; \
             CREATE CAST(integer AS varchar) WITH FUNCTION nested_varchar(integer,integer,boolean) AS ASSIGNMENT; \
             CREATE TABLE nested_values(value varchar(5)); ALTER SEQUENCE nested_checks RESTART WITH 1", true);
        session.success(
            &mut engine,
            "INSERT INTO nested_values VALUES(11),(22)",
            true,
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT value FROM nested_values ORDER BY value",
                true
            )),
            ["11", "22"]
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, "SELECT is_called FROM nested_checks", true)),
            ["f"]
        );
    }
}

#[test]
fn registered_cast_and_operator_inputs_follow_planning_and_runtime_phases() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("cast-operator-input-phases-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE SEQUENCE operator_checks; CREATE TYPE operator_choice AS ENUM('a'); \
             CREATE DOMAIN operator_checked AS operator_choice CHECK(nextval('operator_checks')>0); \
             CREATE TABLE operator_empty(value integer); \
             CREATE FUNCTION operator_cast(integer) RETURNS text LANGUAGE sql STRICT AS \
               $$SELECT format('%s',$1) WHERE cardinality('{a}'::operator_checked[])=1$$; \
             CREATE CAST(integer AS text) WITH FUNCTION operator_cast(integer); \
             CREATE FUNCTION operator_rejected(integer,integer) RETURNS integer LANGUAGE sql AS \
               $$SELECT $1+$2 WHERE cardinality('{a}'::operator_checked[])=1$$; \
             CREATE FUNCTION operator_accepted(integer,integer) RETURNS integer LANGUAGE sql AS \
               $$SELECT $1+$2+CASE WHEN cardinality('{a}'::operator_checked[])=1 THEN 0 ELSE 0 END$$; \
             CREATE FUNCTION operator_opaque(integer,integer) RETURNS integer LANGUAGE sql SET search_path=public AS \
               $$SELECT $1+$2 WHERE cardinality('{a}'::operator_checked[])=1$$; \
             CREATE FUNCTION operator_strict(integer,integer) RETURNS integer LANGUAGE sql STRICT AS \
               $$SELECT $1+$2 WHERE cardinality('{a}'::operator_checked[])=1$$; \
             CREATE FUNCTION operator_unary(integer) RETURNS integer LANGUAGE sql AS \
               $$SELECT $1 WHERE cardinality('{a}'::operator_checked[])=1$$; \
             CREATE OPERATOR #+ (LEFTARG=integer,RIGHTARG=integer,FUNCTION=operator_rejected); \
             CREATE OPERATOR #* (LEFTARG=integer,RIGHTARG=integer,FUNCTION=operator_accepted); \
             CREATE OPERATOR #/ (LEFTARG=integer,RIGHTARG=integer,FUNCTION=operator_opaque); \
             CREATE OPERATOR #? (LEFTARG=integer,RIGHTARG=integer,FUNCTION=operator_strict); \
             CREATE OPERATOR ~# (RIGHTARG=integer,FUNCTION=operator_unary)", true);
        let counter = "SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM operator_checks";
        // The owning differential fixture compares these counts with PostgreSQL.
        for (sql, declared, fetched) in [
            ("SELECT CAST(11 AS text)", "1", "2"),
            ("SELECT 11::text", "1", "2"),
            ("SELECT value::text FROM operator_empty", "1", "1"),
            (
                "SELECT CASE WHEN false THEN 11::text ELSE NULL END",
                "0",
                "0",
            ),
            ("SELECT NULL::integer::text", "0", "0"),
            ("SELECT NULL::integer[]::text[]", "1", "1"),
            ("SELECT ARRAY[]::integer[]::text[]", "1", "1"),
            ("SELECT (ARRAY[11,22]::integer[])::text[]", "1", "2"),
            ("SELECT ARRAY[11,22]::text[]", "2", "3"),
            ("SELECT 1 #+ 2", "1", "2"),
            ("SELECT 1 OPERATOR(public.#+) 2", "1", "2"),
            ("SELECT 1 #* 2", "1", "1"),
            ("SELECT 1 #/ 2", "0", "1"),
            ("SELECT NULL::integer #? 2", "0", "0"),
            ("SELECT value #+ 2 FROM operator_empty", "1", "1"),
            ("SELECT CASE WHEN false THEN 1 #+ 2 ELSE 0 END", "0", "0"),
            ("SELECT ~# 11", "1", "2"),
        ] {
            session.success(
                &mut engine,
                "DISCARD PLANS; ALTER SEQUENCE operator_checks RESTART WITH 1; BEGIN",
                true,
            );
            session.success(
                &mut engine,
                &format!("DECLARE operator_cursor NO SCROLL CURSOR FOR {sql}"),
                true,
            );
            assert_eq!(
                data_rows(&session.success(&mut engine, counter, true)),
                [declared],
                "DECLARE {sql}"
            );
            session.success(&mut engine, "FETCH ALL FROM operator_cursor", true);
            assert_eq!(
                data_rows(&session.success(&mut engine, counter, true)),
                [fetched],
                "FETCH {sql}"
            );
            session.success(&mut engine, "CLOSE operator_cursor; COMMIT", true);
        }
        session.success(&mut engine,
            "CREATE OR REPLACE FUNCTION operator_cast(integer) RETURNS text LANGUAGE sql CALLED ON NULL INPUT AS \
               $$SELECT format('%s',$1)||CASE WHEN cardinality('{a}'::operator_checked[])=1 THEN '' ELSE '' END$$", true);
        for (sql, checks) in [
            ("SELECT ARRAY[11,22]::text[]", "2"),
            ("SELECT ARRAY[11,NULL::integer,22]::text[]", "3"),
            ("SELECT ARRAY[ARRAY[11,22],ARRAY[33,44]]::text[]", "4"),
            (
                "SELECT ARRAY[ARRAY[11,22]::integer[],ARRAY[33,44]::integer[]]::text[]",
                "2",
            ),
            ("SELECT ARRAY[11::text,22]::text[]", "2"),
        ] {
            session.success(
                &mut engine,
                "DISCARD PLANS; ALTER SEQUENCE operator_checks RESTART WITH 1; BEGIN",
                true,
            );
            session.success(
                &mut engine,
                &format!("DECLARE operator_cursor NO SCROLL CURSOR FOR {sql}"),
                true,
            );
            assert_eq!(
                data_rows(&session.success(&mut engine, counter, true)),
                [checks],
                "DECLARE accepted {sql}"
            );
            session.success(&mut engine, "FETCH ALL FROM operator_cursor", true);
            assert_eq!(
                data_rows(&session.success(&mut engine, counter, true)),
                [checks],
                "FETCH accepted {sql}"
            );
            session.success(&mut engine, "CLOSE operator_cursor; COMMIT", true);
        }
    }
}
