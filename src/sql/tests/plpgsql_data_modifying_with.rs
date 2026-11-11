//! Data-modifying `WITH` queries in PL/pgSQL `SELECT ... INTO`, `PERFORM`,
//! `RETURN QUERY` and `FOR` loops, compared with PostgreSQL 18.6.

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
