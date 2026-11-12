use super::*;

fn rows(engine: &mut Engine, budget: &mut Budget, sql: &str) -> Vec<String> {
    let output = run_with(engine, budget, sql);
    let text = String::from_utf8_lossy(&output);
    assert!(error_sqlstates(&output).is_empty(), "{sql}: {text}");
    data_rows(&output)
}

fn sqlstate(engine: &mut Engine, budget: &mut Budget, sql: &str) -> String {
    let output = run_with(engine, budget, sql);
    let states = error_sqlstates(&output);
    assert_eq!(
        states.len(),
        1,
        "{sql}: {}",
        String::from_utf8_lossy(&output)
    );
    states[0].to_string()
}

#[test]
fn relation_row_types_are_values_casts_and_whole_row_types() {
    let (mut engine, mut budget) = test_engine();
    rows(
        &mut engine,
        &mut budget,
        "CREATE TABLE orders(id int primary key, note text)",
    );
    rows(
        &mut engine,
        &mut budget,
        "INSERT INTO orders VALUES (1, 'a'), (2, 'b')",
    );
    for (sql, expected) in [
        ("SELECT ROW(1, 'x')::orders", vec!["(1,x)"]),
        ("SELECT pg_typeof(ROW(1, 'x')::orders)", vec!["orders"]),
        ("SELECT (ROW(1, 'x')::orders).note", vec!["x"]),
        ("SELECT ('(3,c)'::orders).id", vec!["3"]),
        (
            "SELECT pg_typeof(o), o FROM orders o ORDER BY id",
            vec!["orders|(1,a)", "orders|(2,b)"],
        ),
        (
            "SELECT o = ROW(1, 'a')::orders FROM orders o ORDER BY id",
            vec!["t", "f"],
        ),
        // Composite-typed values use the record btree operators: NULL fields
        // compare equal instead of making the result unknown.
        (
            "SELECT ROW(1, NULL)::orders = ROW(1, NULL)::orders, \
                    ROW(1, NULL)::orders < ROW(1, 'a')::orders",
            vec!["t|f"],
        ),
        (
            "SELECT pg_typeof(array_agg(o)), array_agg(o ORDER BY id) FROM orders o",
            vec!["orders[]|{\"(1,a)\",\"(2,b)\"}"],
        ),
        ("SELECT '{\"(1,x)\"}'::orders[]", vec!["{\"(1,x)\"}"]),
        (
            "SELECT typrelid = 'orders'::regclass, oid = 'orders'::regtype FROM pg_type \
             WHERE oid = (SELECT reltype FROM pg_class WHERE relname = 'orders')",
            vec!["t|t"],
        ),
    ] {
        assert_eq!(rows(&mut engine, &mut budget, sql), expected, "{sql}");
    }
    assert_eq!(
        sqlstate(&mut engine, &mut budget, "SELECT ROW(1, 'x', 3)::orders"),
        "42846"
    );
}

#[test]
fn named_composite_array_aggregates_resolve_inside_from_scopes() {
    let (mut engine, mut budget) = test_engine();
    rows(
        &mut engine,
        &mut budget,
        "CREATE TYPE pair AS (id int, note text)",
    );
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT pg_typeof(array_agg(ROW(g, 'x')::pair)), array_agg(ROW(g, 'x')::pair) \
             FROM generate_series(1, 2) g"
        ),
        ["pair[]|{\"(1,x)\",\"(2,x)\"}"]
    );
}

#[test]
fn routine_signatures_accept_relation_row_types_and_depend_on_the_relation() {
    let (mut engine, mut budget) = test_engine();
    for sql in [
        "CREATE TABLE orders(id int, note text)",
        "INSERT INTO orders VALUES (1, 'a'), (2, 'b')",
        "CREATE FUNCTION row_of(i int) RETURNS orders LANGUAGE sql \
         AS 'SELECT * FROM orders WHERE id = i'",
        "CREATE FUNCTION scaled(o orders) RETURNS int LANGUAGE plpgsql \
         AS $$BEGIN RETURN o.id * 10; END$$",
        "CREATE FUNCTION all_orders() RETURNS SETOF orders LANGUAGE sql \
         AS 'SELECT * FROM orders ORDER BY id'",
    ] {
        rows(&mut engine, &mut budget, sql);
    }
    for (sql, expected) in [
        ("SELECT row_of(1), (row_of(2)).note", vec!["(1,a)|b"]),
        (
            "SELECT scaled(o) FROM orders o ORDER BY 1",
            vec!["10", "20"],
        ),
        ("SELECT * FROM all_orders()", vec!["1|a", "2|b"]),
        ("SELECT all_orders()", vec!["(1,a)", "(2,b)"]),
        (
            "SELECT pg_get_function_arguments('scaled'::regproc), \
                    pg_get_function_result('row_of'::regproc)",
            vec!["o orders|orders"],
        ),
    ] {
        assert_eq!(rows(&mut engine, &mut budget, sql), expected, "{sql}");
    }
    assert_eq!(
        sqlstate(&mut engine, &mut budget, "DROP TABLE orders"),
        "2BP01"
    );
    rows(
        &mut engine,
        &mut budget,
        "ALTER TABLE orders RENAME TO sales",
    );
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT pg_get_function_result('row_of'::regproc)"
        ),
        ["sales"]
    );
    rows(&mut engine, &mut budget, "DROP TABLE sales CASCADE");
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT count(*) FROM pg_proc WHERE proname IN ('row_of', 'scaled', 'all_orders')"
        ),
        ["0"]
    );
}

#[test]
fn columns_of_relation_row_types_store_values_and_guard_the_relation() {
    let (mut engine, mut budget) = test_engine();
    for sql in [
        "CREATE TABLE orders(id int, note text, extra int)",
        "CREATE TABLE holder(h int, o orders, oa orders[])",
        "INSERT INTO holder VALUES (1, ROW(1, 'a', 7), ARRAY[ROW(2, 'b', 8)::orders])",
        "ALTER TABLE orders ADD COLUMN q1 int",
        "ALTER TABLE orders DROP COLUMN extra",
        "ALTER TABLE orders RENAME COLUMN note TO memo",
        "INSERT INTO holder VALUES (2, '(3,c,)')",
    ] {
        rows(&mut engine, &mut budget, sql);
    }
    for (sql, expected) in [
        (
            "SELECT * FROM holder ORDER BY h",
            vec!["1|(1,a,)|{\"(2,b,)\"}", "2|(3,c,)|NULL"],
        ),
        (
            "SELECT (o).memo, (oa[1]).memo, pg_typeof(o) FROM holder WHERE h = 1",
            vec!["a|b|orders"],
        ),
        (
            "SELECT h FROM holder WHERE o = ROW(1, 'a', NULL)::orders",
            vec!["1"],
        ),
        (
            "SELECT format_type(atttypid, atttypmod) FROM pg_attribute \
             WHERE attrelid = 'holder'::regclass AND attnum > 1 ORDER BY attnum",
            vec!["orders", "orders[]"],
        ),
    ] {
        assert_eq!(rows(&mut engine, &mut budget, sql), expected, "{sql}");
    }
    for (sql, state) in [
        ("ALTER TABLE orders ADD COLUMN q2 int DEFAULT 5", "0A000"),
        ("ALTER TABLE orders ALTER COLUMN q1 TYPE bigint", "0A000"),
        ("ALTER TABLE orders ADD COLUMN self orders", "42P16"),
        ("ALTER TABLE orders ADD COLUMN nested holder", "42P16"),
        ("DROP TABLE orders", "2BP01"),
    ] {
        assert_eq!(sqlstate(&mut engine, &mut budget, sql), state, "{sql}");
    }
    rows(&mut engine, &mut budget, "DROP TABLE orders CASCADE");
    assert_eq!(
        rows(&mut engine, &mut budget, "SELECT * FROM holder ORDER BY h"),
        ["1", "2"]
    );
}

#[test]
fn relation_row_type_columns_and_signatures_survive_recovery() {
    let config = test_config("relation-row-type-recovery");
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 27));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    for sql in [
        "CREATE TABLE orders(id int, note text)",
        "CREATE TABLE holder(h int, o orders)",
        "INSERT INTO holder VALUES (1, ROW(1, 'a'))",
        "CREATE FUNCTION note_of(o orders) RETURNS text LANGUAGE sql RETURN (o).note",
    ] {
        rows(&mut engine, &mut budget, sql);
    }
    // Journal replay, then a checkpoint image, each restore the bindings.
    for checkpoint in [false, true] {
        if checkpoint {
            rows(&mut engine, &mut budget, "CHECKPOINT");
        }
        drop(engine);
        budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 27));
        engine = Engine::new(&config, &mut budget).unwrap();
        assert_eq!(
            rows(
                &mut engine,
                &mut budget,
                "SELECT pg_typeof(o), note_of(o) FROM holder"
            ),
            ["orders|a"]
        );
        assert_eq!(
            sqlstate(&mut engine, &mut budget, "DROP TABLE orders"),
            "2BP01"
        );
    }
}

#[test]
fn whole_row_constraints_and_indexes_name_depend_and_print_like_postgresql() {
    let (mut engine, mut budget) = test_engine();
    for sql in [
        "CREATE TABLE wr(a int, b text)",
        "CREATE FUNCTION wr_f(wr) RETURNS int LANGUAGE sql IMMUTABLE AS 'select $1.a'",
        "CREATE INDEX wr_i ON wr (wr_f(wr))",
        "ALTER TABLE wr ADD CHECK (wr_f(wr.*) > 0)",
        "ALTER TABLE wr ADD CHECK (wr IS NOT NULL)",
        "ALTER TABLE wr ADD CHECK (ROW(wr.*) IS NOT NULL)",
        "CREATE TABLE ww(a int, b int, c int, CHECK (c > b AND a > 0 AND ww IS NOT NULL))",
        "CREATE VIEW wv AS SELECT wr_f(wr.*) FROM wr",
    ] {
        rows(&mut engine, &mut budget, sql);
    }
    for (sql, expected) in [
        (
            "SELECT conname, conkey::text, pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE conrelid IN ('wr'::regclass, 'ww'::regclass) ORDER BY 1",
            vec![
                "wr_check|{0}|CHECK ((wr_f(wr.*) > 0))",
                "wr_check1|{0}|CHECK ((wr.* IS NOT NULL))",
                "wr_check2|{1,2}|CHECK ((ROW(a, b) IS NOT NULL))",
                "ww_check|{3,2,1,0}|CHECK (((c > b) AND (a > 0) AND (ww.* IS NOT NULL)))",
            ],
        ),
        (
            "SELECT pg_get_indexdef('wr_i'::regclass)",
            vec!["CREATE INDEX wr_i ON public.wr USING btree (wr_f(wr.*))"],
        ),
        (
            "SELECT refobjsubid, deptype FROM pg_depend WHERE objid = 'wr_i'::regclass \
             AND refobjid = 'wr'::regclass",
            vec!["0|a"],
        ),
        ("SELECT * FROM wv", vec![]),
    ] {
        assert_eq!(rows(&mut engine, &mut budget, sql), expected, "{sql}");
    }
    // A row null test reads every field.
    assert_eq!(
        sqlstate(&mut engine, &mut budget, "INSERT INTO wr VALUES (1, NULL)"),
        "23514"
    );
    for (sql, state) in [
        ("CREATE TABLE wq (LIKE wr INCLUDING CONSTRAINTS)", "0A000"),
        ("CREATE TABLE wc () INHERITS (wr)", "0A000"),
        ("CREATE STATISTICS ws ON (wr IS NULL), a FROM wr", "0A000"),
        ("CREATE TABLE wbad(a int, CHECK (zz.a > 0))", "42P01"),
        // A row-typed key has no stored encoding.
        ("CREATE INDEX wr_row ON wr ((wr))", "0A000"),
    ] {
        assert_eq!(sqlstate(&mut engine, &mut budget, sql), state, "{sql}");
    }
    rows(&mut engine, &mut budget, "ALTER TABLE wr RENAME TO wr2");
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE conname = 'wr_check1'",
        ),
        vec!["CHECK ((wr2.* IS NOT NULL))"]
    );
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT pg_get_indexdef('wr_i'::regclass)"
        ),
        vec!["CREATE INDEX wr_i ON public.wr2 USING btree (wr_f(wr2.*))"]
    );
    rows(&mut engine, &mut budget, "INSERT INTO wr2 VALUES (1, 'x')");
}

#[test]
fn index_names_follow_postgresql_column_name_choice() {
    let (mut engine, mut budget) = test_engine();
    rows(
        &mut engine,
        &mut budget,
        "CREATE TABLE k(a int, b text, c int)",
    );
    for sql in [
        "CREATE INDEX ON k (a, (a + 1))",
        "CREATE INDEX ON k (a, a)",
        "CREATE INDEX ON k (a) INCLUDE (b, c)",
        "CREATE INDEX ON k ((a + 1), (a + 2))",
        "CREATE INDEX ON k ((k.c))",
        "CREATE INDEX ON k ((k.a + k.c))",
    ] {
        rows(&mut engine, &mut budget, sql);
    }
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT c.relname, string_agg(a.attname, ',' ORDER BY a.attnum) \
             FROM pg_class c JOIN pg_attribute a ON a.attrelid = c.oid \
             WHERE c.relname LIKE 'k\\_%' AND c.relkind = 'i' GROUP BY 1 ORDER BY 1",
        ),
        vec![
            "k_a_a1_idx|a,a1",
            "k_a_b_c_idx|a,b,c",
            "k_a_expr_idx|a,expr",
            "k_c_idx|c",
            "k_expr_expr1_idx|expr,expr1",
            "k_expr_idx|expr",
        ]
    );
}

#[test]
fn memory_context_logging_answers_only_for_server_processes() {
    let (mut engine, mut budget) = test_engine();
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT pg_log_backend_memory_contexts(1), pg_log_backend_memory_contexts(NULL)",
        ),
        vec!["f|NULL"]
    );
    let output = run_with(
        &mut engine,
        &mut budget,
        "SELECT pg_log_backend_memory_contexts(1)",
    );
    assert!(
        String::from_utf8_lossy(&output).contains("PID 1 is not a PostgreSQL server process"),
        "{}",
        String::from_utf8_lossy(&output)
    );
}
