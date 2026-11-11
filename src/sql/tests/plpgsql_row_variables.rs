//! PL/pgSQL row variables declared with a table's row type, positional row
//! moves and field assignment, and record literal input, compared with
//! PostgreSQL 18.6.

use super::compatibility_diagnostics::{diagnostics, with_engine};
use super::*;

fn notices(engine: &mut Engine, budget: &mut Budget, sql: &str) -> Vec<String> {
    diagnostics(&run_with(engine, budget, sql))
}

#[test]
fn table_names_declare_row_variables_that_move_rows_by_position() {
    with_engine("plpgsql-table-row-variables", |engine, budget| {
        for sql in [
            "CREATE TABLE orders (id int, item text, qty numeric(6,2))",
            "INSERT INTO orders VALUES (1, 'apple', 3), (2, 'pear', 5)",
            "CREATE TYPE pair AS (x int, y text)",
            "CREATE TYPE text_pair AS (x text, y int)",
        ] {
            run_with(engine, budget, sql);
        }
        assert_eq!(
            notices(
                engine,
                budget,
                "DO $$ DECLARE r orders; BEGIN SELECT * INTO r FROM orders WHERE id = 1; \
                 RAISE NOTICE '% %', r.item, r; END $$",
            ),
            ["NOTICE 00000 apple (1,apple,3.00)", "DO"]
        );
        assert_eq!(
            notices(
                engine,
                budget,
                "DO $$ DECLARE r public.orders%ROWTYPE; BEGIN r.id := 7; r.qty := '2.555'; \
                 RAISE NOTICE '%', r; r.id := r.id + 1; RAISE NOTICE '%', r; END $$",
            ),
            ["NOTICE 00000 (7,,2.56)", "NOTICE 00000 (8,,2.56)", "DO"]
        );
        // Missing fields become NULL, extra ones are ignored, and each field
        // converts through text when no assignment cast exists.
        assert_eq!(
            notices(
                engine,
                budget,
                "DO $$ DECLARE p pair; t text_pair; r orders; BEGIN \
                 SELECT 1, 'a', 3 INTO p; RAISE NOTICE '%', p; \
                 SELECT 1 INTO p; RAISE NOTICE '%', p; \
                 SELECT ROW(1, 2)::pair INTO t; RAISE NOTICE '%', t; \
                 SELECT 1.5, 2 INTO r; RAISE NOTICE '%', r; \
                 p := ROW('7', 8); RAISE NOTICE '%', p; END $$",
            ),
            [
                "NOTICE 00000 (1,a)",
                "NOTICE 00000 (1,)",
                r#"NOTICE 00000 ("(1,2)",)"#,
                "NOTICE 00000 (2,2,)",
                "NOTICE 00000 (7,8)",
                "DO",
            ]
        );
        // One composite column reaches the first field, as in PostgreSQL.
        assert_eq!(
            notices(
                engine,
                budget,
                "DO $$ DECLARE r orders; BEGIN SELECT o INTO r FROM orders o WHERE id = 1; END $$",
            )[0],
            r#"ERROR 22P02 invalid input syntax for type integer: "(1,apple,3.00)""#
        );
        assert_eq!(
            notices(
                engine,
                budget,
                "DO $$ DECLARE r orders; BEGIN r.nope := 1; END $$",
            )[0],
            r#"ERROR 42703 record "r" has no field "nope""#
        );
        assert_eq!(
            notices(
                engine,
                budget,
                "DO $$ DECLARE p record; BEGIN p.x := 1; END $$",
            )[0],
            r#"ERROR 55000 record "p" is not assigned yet detail: The tuple structure of a not-yet-assigned record is indeterminate."#
        );
        // Reading a field of an unassigned record variable fails the same way.
        assert_eq!(
            notices(
                engine,
                budget,
                "DO $$ DECLARE p record; BEGIN RAISE NOTICE '%', p.x; END $$",
            )[0],
            r#"ERROR 55000 record "p" is not assigned yet detail: The tuple structure of a not-yet-assigned record is indeterminate."#
        );
        assert_eq!(
            notices(
                engine,
                budget,
                "DO $$ DECLARE p record; BEGIN SELECT 1 AS x INTO p; p.x := '42'; \
                 RAISE NOTICE '%', p; END $$",
            ),
            ["NOTICE 00000 (42)", "DO"]
        );
    });
}

#[test]
fn record_literals_follow_record_input() {
    with_engine("record-literal-input", |engine, budget| {
        run_with(engine, budget, "CREATE TYPE pair AS (x int, y text)");
        let output = run_with(
            engine,
            budget,
            r#"SELECT ' ( 3 , "c d" ) '::pair, '(,)'::pair, '(1,"")'::pair, '(1,a"b,c"d)'::pair, '(1,a\,b)'::pair"#,
        );
        assert_eq!(
            data_rows(&output),
            [r#"(3," c d ")|(,)|(1,"")|(1,"ab,cd")|(1,"a,b")"#]
        );
        for (literal, detail) in [
            ("5", "Missing left parenthesis."),
            ("(3", "Unexpected end of input."),
            ("(3,c)x", "Junk after right parenthesis."),
            ("(1,2,3)", "Too many columns."),
            ("(1)", "Too few columns."),
        ] {
            assert_eq!(
                diagnostics(&run_with(
                    engine,
                    budget,
                    &format!("SELECT '{literal}'::pair")
                )),
                [format!(
                    r#"ERROR 22P02 malformed record literal: "{literal}" @8 detail: {detail}"#
                )]
            );
        }
        assert_eq!(
            diagnostics(&run_with(engine, budget, "SELECT 5::pair")),
            ["ERROR 42846 cannot cast type integer to pair @9"]
        );
    });
}
