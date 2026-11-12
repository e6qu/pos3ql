use super::*;

fn rows(engine: &mut Engine, budget: &mut Budget, sql: &str) -> Vec<String> {
    let output = run_with(engine, budget, sql);
    assert!(
        error_sqlstates(&output).is_empty(),
        "{sql}: {}",
        String::from_utf8_lossy(&output)
    );
    data_rows(&output)
}

/// `p.f` in a SQL function body is field `f` of composite parameter `p`
/// unless a FROM item `p` has a column `f`, in every typing pass.
#[test]
fn sql_function_composite_parameter_fields_yield_to_columns() {
    let (mut engine, mut budget) = test_engine();
    rows(
        &mut engine,
        &mut budget,
        "CREATE TYPE pair AS (id integer, note text); \
         CREATE TABLE shadow(id integer, note text); INSERT INTO shadow VALUES (1, 'column'); \
         CREATE FUNCTION field(o pair) RETURNS text LANGUAGE sql AS 'SELECT o.note'; \
         CREATE FUNCTION qualified(o pair) RETURNS text LANGUAGE sql \
           AS 'SELECT qualified.o.note || o.id'; \
         CREATE FUNCTION shadowed(o pair) RETURNS text LANGUAGE sql \
           AS 'SELECT o.note FROM shadow o'; \
         CREATE FUNCTION filtered(o pair) RETURNS text LANGUAGE sql \
           RETURN (SELECT s.note || o.note FROM shadow s WHERE s.id = o.id)",
    );
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT field(ROW(1, 'p')::pair), qualified(ROW(2, 'q')::pair), \
             shadowed(ROW(1, 'p')::pair), filtered(ROW(1, 'p')::pair), \
             pg_typeof(field(NULL::pair))"
        ),
        ["p|q2|column|columnp|text"]
    );
    let output = run_with(
        &mut engine,
        &mut budget,
        "CREATE FUNCTION missing(o pair) RETURNS text LANGUAGE sql AS 'SELECT o.nothing'",
    );
    assert_eq!(error_sqlstates(&output), ["42P01"]);
}

/// Named composites and relation rows convert to JSON objects and coerce to
/// `record` arguments.
#[test]
fn named_composites_convert_to_json_objects() {
    let (mut engine, mut budget) = test_engine();
    rows(
        &mut engine,
        &mut budget,
        "CREATE TYPE inner_pair AS (a integer); \
         CREATE TABLE outer_rows(a integer, c inner_pair); \
         INSERT INTO outer_rows VALUES (1, ROW(2))",
    );
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT row_to_json(r), to_jsonb(r), row_to_json(r, true) FROM outer_rows r"
        ),
        ["{\"a\":1,\"c\":{\"a\":2}}|{\"a\": 1, \"c\": {\"a\": 2}}|{\"a\":1,\n \"c\":{\"a\":2}}"]
    );
}

/// `$1.field` selects a field of a positional composite parameter, as
/// PostgreSQL's `PARAM opt_indirection` grammar allows.
#[test]
fn positional_parameter_fields_parse_and_run() {
    let (mut engine, mut budget) = test_engine();
    rows(
        &mut engine,
        &mut budget,
        "CREATE TABLE pair(a integer, b text); \
         CREATE FUNCTION first_of(pair) RETURNS integer LANGUAGE sql AS 'SELECT $1.a'; \
         CREATE FUNCTION second_of(pair) RETURNS text LANGUAGE sql AS 'SELECT $1.b || ''z'''",
    );
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT first_of(ROW(3, 'x')::pair), second_of(ROW(4, 'y')::pair)",
        ),
        ["3|yz"]
    );
}
