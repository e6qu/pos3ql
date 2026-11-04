use super::*;

#[test]
fn typed_selection_competes_across_namespaces_without_hiding_better_overloads() {
    let config = test_config("typed-routine-selection");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine,
        "CREATE FUNCTION public.unnest(integer[]) RETURNS SETOF integer LANGUAGE sql IMMUTABLE AS 'SELECT 99'", true);
    for path in ["pg_catalog,public", "public,pg_catalog"] {
        session.success(&mut engine, &format!("SET search_path={path}"), true);
        assert_eq!(
            data_rows(&session.success(&mut engine, "SELECT unnest(ARRAY[1,2])", true)),
            ["99"]
        );
        let selected = crate::mem::guard::forbid_alloc(|| {
            engine.storage.select_routine_call(
                "unnest",
                &[],
                &[crate::sql::types::ArrElem::Int4.array_oid()],
                false,
                0,
                crate::storage::RoutineCallKind::Function,
            )
        });
        assert!(matches!(
            selected,
            crate::storage::RoutineCallSelection::Stored(_)
        ));
    }
    session.success(&mut engine,
        "DROP FUNCTION public.unnest(integer[]); CREATE FUNCTION public.unnest(bigint[]) RETURNS SETOF bigint LANGUAGE sql IMMUTABLE AS 'SELECT 99'", true);
    for path in ["pg_catalog,public", "public,pg_catalog"] {
        session.success(&mut engine, &format!("SET search_path={path}"), true);
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, "SELECT unnest(ARRAY[1,2])", true)),
            ["42725"]
        );
    }
    session.success(&mut engine,
        "DROP FUNCTION public.unnest(bigint[]); CREATE FUNCTION public.unnest(anyarray) RETURNS SETOF anyelement LANGUAGE sql IMMUTABLE AS 'SELECT $1[2]'", true);
    session.success(&mut engine, "SET search_path=pg_catalog,public", true);
    assert_eq!(
        data_rows(&session.success(&mut engine, "SELECT unnest(ARRAY[1,2])", true)),
        ["1", "2"]
    );
    session.success(&mut engine, "SET search_path=public,pg_catalog", true);
    assert_eq!(
        data_rows(&session.success(&mut engine, "SELECT unnest(ARRAY[1,2])", true)),
        ["2"]
    );
}

#[test]
fn call_selection_keeps_output_placeholders_and_wrong_kind_errors() {
    let config = test_config("typed-procedure-selection");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine,
        "CREATE SCHEMA procedure_choices; CREATE TABLE procedure_effects(value integer); \
         CREATE SEQUENCE procedure_arguments; \
         CREATE FUNCTION public.call_choice(integer) RETURNS integer LANGUAGE sql AS 'SELECT $1'; \
         CREATE PROCEDURE procedure_choices.call_choice(value integer) LANGUAGE sql AS 'INSERT INTO public.procedure_effects VALUES($1)'; \
         CREATE PROCEDURE procedure_choices.output_choice(IN value integer,OUT result integer) LANGUAGE sql AS 'SELECT $1+1'; \
         SET search_path=public,procedure_choices,pg_catalog", true);
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "CALL call_choice(nextval('procedure_arguments')::integer)",
            true
        )),
        ["42809"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "SELECT nextval('procedure_arguments')", true)),
        ["1"]
    );
    session.success(
        &mut engine,
        "SET search_path=procedure_choices,public,pg_catalog; CALL call_choice(7)",
        true,
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT value FROM procedure_effects; CALL output_choice(8,NULL)",
            true
        )),
        ["7", "9"]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "CALL output_choice(result=>NULL,value=>10)",
            true
        )),
        ["11"]
    );
}

#[test]
fn stored_aggregate_hooks_precede_scalar_routine_execution() {
    let config = test_config("stored-aggregate-hook-priority");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine,
        "CREATE FUNCTION aggregate_state(state bigint,value integer) RETURNS bigint LANGUAGE sql AS 'SELECT coalesce(state,0)+value'; \
         CREATE FUNCTION aggregate_final(state bigint) RETURNS bigint LANGUAGE sql AS 'SELECT state*2'; \
         CREATE FUNCTION aggregate_first_state(state anyelement,value anyelement) RETURNS anyelement LANGUAGE sql AS 'SELECT coalesce(state,value)'; \
         CREATE FUNCTION aggregate_compatible_state(state anycompatible,value anycompatible) RETURNS anycompatible LANGUAGE sql AS 'SELECT coalesce(state,value)'; \
         CREATE AGGREGATE aggregate_total(integer)(SFUNC=aggregate_state,STYPE=bigint,FINALFUNC=aggregate_final); \
         CREATE AGGREGATE aggregate_first(anyelement)(SFUNC=aggregate_first_state,STYPE=anyelement); \
         CREATE AGGREGATE aggregate_compatible(anycompatible)(SFUNC=aggregate_compatible_state,STYPE=anycompatible); \
         CREATE SEQUENCE aggregate_inputs START WITH 7", true);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT aggregate_total(7),aggregate_first(9)",
            true
        )),
        ["14|9"]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT aggregate_first_state(NULL::text,'x'::text), \
                    aggregate_first('y'::text),aggregate_first(ARRAY['z'::text])::text, \
                    aggregate_compatible('c'::text)",
            true
        )),
        ["x|y|{z}|c"]
    );
    crate::mem::guard::forbid_alloc(|| {
        use crate::sql::types::oid;
        assert!(
            !engine
                .storage
                .routine_implicit_cast(oid::TEXT, oid::ANYELEMENT, 0)
        );
        assert!(
            !engine
                .storage
                .routine_implicit_cast(oid::ANYELEMENT, oid::TEXT, 0)
        );
        assert!(
            engine
                .storage
                .routine_implicit_cast(oid::ANYELEMENT, oid::ANYELEMENT, 0)
        );
    });
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT aggregate_total(nextval('aggregate_inputs')::integer); SELECT nextval('aggregate_inputs')", true)), ["14", "8"]);
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT value,aggregate_total(value) OVER(ORDER BY value ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM (VALUES(1),(2)) AS inputs(value) ORDER BY value", true)), ["1|2", "2|6"]);
}

#[test]
fn ordered_percentiles_keep_numeric_interval_array_and_direct_input_contracts() {
    let config = test_config("ordered-percentile-types");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let sql =
        include_str!("../../../tests/external/differential_exact/64_ordered_percentile_types.sql");
    let mut requests = sql.split("-- next request");
    assert_eq!(
        data_rows(&session.success(&mut engine, requests.next().unwrap(), true)),
        [
            "2|double precision|4000000000000000|1.0|1.0",
            "1 mon 16 days 12:00:00.000003|interval",
            "[0:3]={1,2,3,NULL}|double precision[]|{1.0,1.0,3.0,NULL}",
            "{\"1 day\",\"2 days\",\"3 days\",NULL}",
            "NULL|NULL|{}",
            "NULL",
            "{3,1}|3.0|3.0",
            "0|1",
            "1|3",
            "1",
            "2",
        ]
    );
    for state in ["22003", "22003", "22003", "22008"] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, requests.next().unwrap(), true)),
            [state]
        );
    }
    session.success(&mut engine, requests.next().unwrap(), true);
    assert!(requests.next().is_none());
    for sql in [
        "SELECT INTERVAL'2147483647 months'+INTERVAL'1 month'",
        "SELECT INTERVAL'infinity'-INTERVAL'infinity'",
        "SELECT INTERVAL'infinity'+INTERVAL'-infinity'",
        "SELECT INTERVAL'1 month'*1e30::float8",
        "SELECT INTERVAL'1 month'*'NaN'::float8",
        "SELECT INTERVAL'0'*'Infinity'::float8",
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            ["22008"],
            "{sql}"
        );
    }
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "SELECT INTERVAL'1 day'/0", true)),
        ["22012"]
    );
}
