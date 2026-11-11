//! `WITH` queries in PL/pgSQL statements, writes, `EXECUTE` and FOR
//! queries, and data-modifying ones in SQL-language functions, compared
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
fn plpgsql_writes_run_with_queries_in_one_command() {
    with_engine("plpgsql-write-with", |engine, budget| {
        for sql in [
            "CREATE TABLE t (a int, b int)",
            "INSERT INTO t VALUES (1, 10), (2, 20)",
        ] {
            run_with(engine, budget, sql);
        }
        // WITH queries read the routine's variables and the rows earlier
        // statements wrote; the write does not see their changes.
        assert_eq!(
            diagnostics(&run_with(
                engine,
                budget,
                "DO $$ DECLARE n int := 5; x int; BEGIN \
                 WITH s AS (SELECT a FROM t WHERE a = 1) UPDATE t SET b = b + n FROM s WHERE t.a = s.a; \
                 WITH s AS (INSERT INTO t VALUES (3, n) RETURNING a) INSERT INTO t SELECT a + 10, n FROM s; \
                 WITH d AS (DELETE FROM t WHERE a = 13 RETURNING a) DELETE FROM t WHERE a IN (SELECT a - 10 FROM d); \
                 WITH i AS (INSERT INTO t VALUES (80, 0) RETURNING a) UPDATE t SET b = b + 1 WHERE a >= 80; \
                 WITH i AS (INSERT INTO t VALUES (81, 0) RETURNING a) SELECT count(*) INTO x FROM t WHERE a = 81; \
                 RAISE NOTICE 'seen %', x; END $$",
            )),
            ["NOTICE 00000 seen 0", "DO"]
        );
        assert_eq!(
            data_rows(&run_with(engine, budget, "SELECT * FROM t ORDER BY a")),
            ["1|15", "2|20", "80|0", "81|0"]
        );
        assert_eq!(
            diagnostics(&run_with(
                engine,
                budget,
                "DO $$ DECLARE n int := 1; x int; BEGIN \
                 EXECUTE 'WITH s AS (SELECT count(*)::int AS c FROM t WHERE a > $1) SELECT c FROM s' INTO x USING n; \
                 EXECUTE 'WITH s AS (SELECT 1 AS a) INSERT INTO t SELECT a + 40, $1 FROM s' USING n; \
                 RAISE NOTICE 'count %', x; END $$",
            )),
            ["NOTICE 00000 count 3", "DO"]
        );
        assert_eq!(
            data_rows(&run_with(engine, budget, "SELECT b FROM t WHERE a = 41")),
            ["1"]
        );
        // A cursor reruns its query at each FETCH and cannot keep the rows
        // its WITH queries wrote.
        assert_eq!(
            diagnostics(&run_with(
                engine,
                budget,
                "DO $$ DECLARE c refcursor; BEGIN \
                 OPEN c FOR WITH i AS (INSERT INTO t VALUES (9, 9) RETURNING a) SELECT a FROM i; END $$",
            ))[0],
            "ERROR 0A000 OPEN of a query with data-modifying WITH queries is not supported"
        );
    });
}

#[test]
fn trigger_functions_run_data_modifying_with_queries() {
    with_engine("trigger-write-with", |engine, budget| {
        for sql in [
            "CREATE TABLE t (a int)",
            "CREATE TABLE log (a int)",
            "CREATE FUNCTION audit() RETURNS trigger LANGUAGE plpgsql AS $$ \
             DECLARE x int; BEGIN \
             WITH s AS (INSERT INTO log VALUES (NEW.a) RETURNING a) INSERT INTO log SELECT a + 1 FROM s; \
             WITH d AS (DELETE FROM log WHERE a = NEW.a + 1 RETURNING a) SELECT count(*) INTO x FROM d; \
             RAISE NOTICE 'deleted %', x; RETURN NEW; END $$",
            "CREATE TRIGGER audited BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION audit()",
        ] {
            run_with(engine, budget, sql);
        }
        assert_eq!(
            diagnostics(&run_with(engine, budget, "INSERT INTO t VALUES (5), (7)")),
            [
                "NOTICE 00000 deleted 1",
                "NOTICE 00000 deleted 1",
                "INSERT 0 2"
            ]
        );
        assert_eq!(
            data_rows(&run_with(engine, budget, "SELECT a FROM log ORDER BY a")),
            ["5", "7"]
        );
    });
}
