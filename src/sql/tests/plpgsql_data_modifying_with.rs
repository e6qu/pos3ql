//! Data-modifying `WITH` queries in PL/pgSQL `SELECT ... INTO`, `PERFORM`,
//! `RETURN QUERY` and `FOR` loops, and in SQL-language functions, compared
//! with PostgreSQL 18.6.

use super::compatibility_diagnostics::{diagnostics, with_engine};
use super::*;

#[test]
fn plpgsql_queries_run_data_modifying_with_queries_once() {
    with_engine("plpgsql-data-modifying-with", |engine, budget| {
        for sql in [
            "CREATE TABLE w (id int, v text)",
            "INSERT INTO w VALUES (1, 'a')",
            "CREATE FUNCTION removed() RETURNS SETOF int LANGUAGE plpgsql AS $$ BEGIN \
             RETURN QUERY WITH d AS (DELETE FROM w WHERE id > 2 RETURNING id) \
             SELECT id FROM d ORDER BY id; END $$",
        ] {
            run_with(engine, budget, sql);
        }
        // The main query reads the table as it was before the WITH writes.
        assert_eq!(
            diagnostics(&run_with(
                engine,
                budget,
                "DO $$ DECLARE n int; m int; BEGIN \
                 WITH x AS (INSERT INTO w VALUES (2, 'b'), (3, 'c'), (4, 'd') RETURNING id) \
                 SELECT count(*), (SELECT count(*) FROM w) INTO n, m FROM x; \
                 RAISE NOTICE 'n=% m=%', n, m; END $$",
            )),
            ["NOTICE 00000 n=3 m=1", "DO"]
        );
        // A WITH query nothing reads still runs, after the main query.
        assert_eq!(
            diagnostics(&run_with(
                engine,
                budget,
                "DO $$ DECLARE n int; BEGIN \
                 WITH x AS (DELETE FROM w WHERE id = 4 RETURNING id), \
                 y AS (UPDATE w SET v = 'u' WHERE id = 1 RETURNING 1) \
                 SELECT max(id) INTO n FROM x; RAISE NOTICE 'n=%', n; END $$",
            )),
            ["NOTICE 00000 n=4", "DO"]
        );
        assert_eq!(
            data_rows(&run_with(engine, budget, "SELECT * FROM removed()")),
            ["3"]
        );
        // A FOR loop reads the WITH query's rows, not the table it changed.
        assert_eq!(
            diagnostics(&run_with(
                engine,
                budget,
                "DO $$ DECLARE r record; BEGIN FOR r IN \
                 WITH x AS (INSERT INTO w VALUES (5, 'e'), (6, 'f') RETURNING id) \
                 SELECT id, (SELECT count(*) FROM w) AS c FROM x ORDER BY id LOOP \
                 RAISE NOTICE 'r=% c=%', r.id, r.c; END LOOP; END $$",
            )),
            ["NOTICE 00000 r=5 c=2", "NOTICE 00000 r=6 c=2", "DO"]
        );
        assert_eq!(
            data_rows(&run_with(engine, budget, "SELECT * FROM w ORDER BY id")),
            ["1|u", "2|b", "5|e", "6|f"]
        );
    });
}

#[test]
fn sql_functions_run_data_modifying_with_queries_once() {
    with_engine("sql-function-data-modifying-with", |engine, budget| {
        for sql in [
            "CREATE TABLE f (a int)",
            "CREATE FUNCTION added(x int) RETURNS SETOF int LANGUAGE sql AS $$ \
             WITH q AS (INSERT INTO f VALUES (x), (x + 1) RETURNING a) \
             SELECT a * 10 FROM q ORDER BY 1 $$",
            "CREATE FUNCTION unread(x int) RETURNS int LANGUAGE sql AS $$ \
             WITH q AS (DELETE FROM f WHERE a = x RETURNING a) SELECT 7 $$",
        ] {
            run_with(engine, budget, sql);
        }
        assert_eq!(
            data_rows(&run_with(engine, budget, "SELECT * FROM added(5)")),
            ["50", "60"]
        );
        // A WITH query nothing reads still runs, after the main query.
        assert_eq!(
            data_rows(&run_with(engine, budget, "SELECT unread(5)")),
            ["7"]
        );
        assert_eq!(
            data_rows(&run_with(engine, budget, "SELECT a FROM f ORDER BY a")),
            ["6"]
        );
    });
}

#[test]
fn plpgsql_statements_read_ordinary_with_queries() {
    with_engine("plpgsql-ordinary-with", |engine, budget| {
        run_with(
            engine,
            budget,
            "CREATE FUNCTION listed() RETURNS SETOF int LANGUAGE plpgsql AS $$ BEGIN \
             RETURN QUERY WITH c AS (SELECT 1 UNION ALL SELECT 2) SELECT * FROM c; END $$",
        );
        assert_eq!(
            data_rows(&run_with(engine, budget, "SELECT * FROM listed()")),
            ["1", "2"]
        );
        assert_eq!(
            diagnostics(&run_with(
                engine,
                budget,
                "DO $$ DECLARE r record; t int := 0; n int; BEGIN \
                 FOR r IN WITH c AS (SELECT g FROM generate_series(1, 3) g) SELECT g FROM c \
                 LOOP t := t + r.g; END LOOP; \
                 WITH c AS MATERIALIZED (SELECT 4 v) SELECT v INTO n FROM c; \
                 RAISE NOTICE '% %', t, n; END $$",
            )),
            ["NOTICE 00000 6 4", "DO"]
        );
    });
}

#[test]
fn plpgsql_with_queries_read_routine_variables() {
    with_engine("plpgsql-with-variables", |engine, budget| {
        run_with(
            engine,
            budget,
            "CREATE FUNCTION wv_rows(n int) RETURNS SETOF int LANGUAGE plpgsql AS $$ BEGIN \
             RETURN QUERY WITH c AS MATERIALIZED (SELECT n AS v UNION ALL SELECT n + 1) \
             SELECT v FROM c ORDER BY v; END $$",
        );
        assert_eq!(
            data_rows(&run_with(engine, budget, "SELECT * FROM wv_rows(5)")),
            ["5", "6"]
        );
        // Results from PostgreSQL 18.6.
        assert_eq!(
            diagnostics(&run_with(
                engine,
                budget,
                "DO $$ DECLARE k int := 7; t text := 'x'; r record; s int; BEGIN \
                 WITH c AS MATERIALIZED (SELECT k * 2 AS v) SELECT v INTO s FROM c; \
                 RAISE NOTICE 's=%', s; \
                 WITH c AS (SELECT k + 1 AS v, t || 'y' AS w) \
                 SELECT c.v + d.v INTO s FROM c, c AS d; RAISE NOTICE 's=%', s; \
                 FOR r IN WITH c AS MATERIALIZED (SELECT k - 1 AS v) SELECT v FROM c LOOP \
                 RAISE NOTICE 'r=%', r.v; END LOOP; END $$",
            )),
            [
                "NOTICE 00000 s=14",
                "NOTICE 00000 s=16",
                "NOTICE 00000 r=6",
                "DO"
            ]
        );
        // A WITH query still cannot read the columns of the query reading it.
        assert_eq!(
            diagnostics(&run_with(
                engine,
                budget,
                "DO $$ DECLARE s int; BEGIN WITH c AS MATERIALIZED (SELECT g AS v) \
                 SELECT max(v) INTO s FROM c, generate_series(1, 2) g; END $$",
            ))[0],
            "ERROR 42703 column \"g\" does not exist"
        );
    });
}
