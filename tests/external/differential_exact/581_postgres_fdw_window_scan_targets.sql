-- A foreign scan under a window returns the window's input, as any
-- relation scan there does, and every WINDOW definition's keys are read,
-- used or not; only a used window's keys order the window input.
CREATE EXTENSION IF NOT EXISTS postgres_fdw;
CREATE SERVER pw_s FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host '127.0.0.1', port '1', dbname 'none');
CREATE USER MAPPING FOR CURRENT_USER SERVER pw_s OPTIONS (user 'nobody');
CREATE FOREIGN TABLE pw_t (id int, a int, s text, n numeric) SERVER pw_s OPTIONS (schema_name 'remote', table_name 't');
CREATE TABLE pw_l (id int, a int, s text, n numeric);
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, rank() OVER (ORDER BY a) FROM pw_t;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, sum(a) OVER (PARTITION BY s) FROM pw_t;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, sum(a) OVER (PARTITION BY s ORDER BY id) FROM pw_t WHERE a > 1;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, row_number() OVER () FROM pw_t;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, count(*) OVER w, max(a) OVER w FROM pw_t WINDOW w AS (ORDER BY s);
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM pw_t WINDOW w AS (ORDER BY s) ORDER BY a LIMIT 2;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, sum(a) OVER w FROM pw_t WINDOW w AS (PARTITION BY s ORDER BY id), u AS (ORDER BY n);
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, sum(a) OVER w FROM pw_l WINDOW w AS (PARTITION BY s ORDER BY id), u AS (ORDER BY n);
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM pw_l WINDOW w AS (ORDER BY s) ORDER BY a LIMIT 2;
-- A local aggregate reads the scan's whole row, as over a table.
EXPLAIN (VERBOSE, COSTS OFF) SELECT count(*) FROM pw_t WHERE random() > 0.5;
EXPLAIN (VERBOSE, COSTS OFF) SELECT sum(a) FROM pw_t WHERE random() > 0.5;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(*) FROM pw_t WHERE random() > 0.5 GROUP BY a;
DROP TABLE pw_l;
DROP SERVER pw_s CASCADE;
