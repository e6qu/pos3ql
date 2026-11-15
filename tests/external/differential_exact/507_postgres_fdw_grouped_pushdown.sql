-- postgres_fdw ships a query's grouping and aggregates when every select-list
-- expression, GROUP BY key, HAVING condition and ORDER BY key ships and the
-- WHERE clause ships whole: the plan is one Foreign Scan of the grouped
-- relation.
CREATE EXTENSION IF NOT EXISTS postgres_fdw;
CREATE SERVER gp_s FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host '127.0.0.1', port '1', dbname 'none');
CREATE USER MAPPING FOR CURRENT_USER SERVER gp_s OPTIONS (user 'nobody');
CREATE FOREIGN TABLE gp_t (id int, a int, s text, n numeric, f float8, b bigint, d date, flag boolean)
  SERVER gp_s OPTIONS (schema_name 'remote', table_name 't');
EXPLAIN (VERBOSE, COSTS OFF) SELECT count(*) FROM gp_t;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(*), sum(id), avg(id), max(s), min(d), sum(n), avg(f), sum(b) FROM gp_t GROUP BY a;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(*) FROM gp_t GROUP BY a HAVING count(*) > 3;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a + 1, count(*) FROM gp_t WHERE id > 2 GROUP BY a + 1 ORDER BY 1;
EXPLAIN (VERBOSE, COSTS OFF) SELECT count(*) FROM gp_t GROUP BY a;
EXPLAIN (VERBOSE, COSTS OFF) SELECT count(*), a FROM gp_t GROUP BY a, s;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(*) * 2 FROM gp_t GROUP BY a;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(*) FROM gp_t GROUP BY a ORDER BY count(*) DESC, a LIMIT 2;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(DISTINCT s), sum(id) FILTER (WHERE id > 3), string_agg(s, ',' ORDER BY s DESC) FROM gp_t GROUP BY a;
EXPLAIN (VERBOSE, COSTS OFF) SELECT t.a, min(t.s) FROM gp_t t GROUP BY t.a;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(*) FROM gp_t GROUP BY a HAVING a > 1 AND count(*) > 1;
EXPLAIN (VERBOSE, COSTS OFF) SELECT count(*), sum(a) FROM gp_t WHERE id < 10;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a FROM gp_t GROUP BY a;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(*) FROM gp_t GROUP BY 1 ORDER BY 2;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(*) FROM gp_t GROUP BY a ORDER BY sum(id);
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(*) c FROM gp_t GROUP BY a ORDER BY c LIMIT 1;
EXPLAIN (VERBOSE, COSTS OFF) SELECT count(*) FROM gp_t LIMIT 1;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(*) FROM gp_t WHERE s = 'x' AND id > 2 GROUP BY a;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, min(s) FROM gp_t GROUP BY s, a;
EXPLAIN (VERBOSE, COSTS OFF) SELECT count(*) FROM gp_t HAVING count(*) > 1;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, bool_and(flag), every(id < 15), bool_or(s = 's3') FROM gp_t GROUP BY a;
EXPLAIN (FORMAT JSON, VERBOSE, COSTS OFF) SELECT a, count(*) FROM gp_t GROUP BY a;
DROP SERVER gp_s CASCADE;
