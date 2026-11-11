//! Data-modifying `WITH` queries in PL/pgSQL `SELECT ... INTO`, `PERFORM`
//! and `RETURN QUERY`, and in SQL-language functions, compared with
//! PostgreSQL 18.6.

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
        assert_eq!(
            data_rows(&run_with(engine, budget, "SELECT * FROM w ORDER BY id")),
            ["1|u", "2|b"]
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
