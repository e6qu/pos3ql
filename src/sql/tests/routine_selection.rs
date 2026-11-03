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
