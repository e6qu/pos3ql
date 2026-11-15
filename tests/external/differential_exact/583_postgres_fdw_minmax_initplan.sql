-- A lone min or max of a foreign table reads the first row of the remote
-- scan ordered by its argument, nulls excluded, as an InitPlan
-- (preprocess_minmax_aggregates); two or more ship as one grouped scan.
CREATE EXTENSION IF NOT EXISTS postgres_fdw;
CREATE SERVER mm_s FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host '127.0.0.1', port '1', dbname 'none');
CREATE USER MAPPING FOR CURRENT_USER SERVER mm_s OPTIONS (user 'nobody');
CREATE FOREIGN TABLE mm_t (id int, a int, s text, n numeric) SERVER mm_s OPTIONS (schema_name 'remote', table_name 't');
EXPLAIN (VERBOSE, COSTS OFF) SELECT min(a) FROM mm_t;
EXPLAIN (VERBOSE, COSTS OFF) SELECT max(a) + 1 FROM mm_t WHERE id > 3;
EXPLAIN (VERBOSE, COSTS OFF) SELECT max(s), max(s) || 'x' FROM mm_t t WHERE t.n < 2;
EXPLAIN (VERBOSE, COSTS OFF) SELECT min(a + id) FROM mm_t;
EXPLAIN (VERBOSE, COSTS OFF) SELECT min(a), max(a) FROM mm_t;
EXPLAIN (VERBOSE, COSTS OFF) SELECT min(a), count(*) FROM mm_t;
EXPLAIN (VERBOSE, COSTS OFF) SELECT min(a) FROM mm_t WHERE random() > 0.5;
EXPLAIN (VERBOSE, COSTS OFF) SELECT min(a) FROM mm_t GROUP BY s;
EXPLAIN (COSTS OFF) SELECT min(n) FROM mm_t;
EXPLAIN (VERBOSE, COSTS OFF) SELECT (SELECT a FROM mm_t WHERE a IS NOT NULL ORDER BY a LIMIT 1);
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM (SELECT id FROM mm_t ORDER BY id LIMIT 3) s;
DROP SERVER mm_s CASCADE;
