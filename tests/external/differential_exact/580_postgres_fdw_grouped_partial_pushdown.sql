-- postgres_fdw ships a query's grouping when every GROUP BY key ships and
-- the WHERE clause ships whole: a select-list constant or star ships, any
-- other expression that does not is computed over the remote groups from
-- the aggregates it reads, a HAVING condition that does not ship filters
-- them, and an order only this side computes sorts them above the scan.
CREATE EXTENSION IF NOT EXISTS postgres_fdw;
CREATE SERVER gl_s FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host '127.0.0.1', port '1', dbname 'none');
CREATE USER MAPPING FOR CURRENT_USER SERVER gl_s OPTIONS (user 'nobody');
CREATE FOREIGN TABLE gl_t (id int, a int, s text, n numeric) SERVER gl_s OPTIONS (schema_name 'remote', table_name 't');
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(*), random() FROM gl_t GROUP BY a;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(*) * random() FROM gl_t GROUP BY a;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a + random(), sum(id) FROM gl_t GROUP BY a;
EXPLAIN (VERBOSE, COSTS OFF) SELECT 1, count(*) FROM gl_t GROUP BY a;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, 'x', count(*) FROM gl_t GROUP BY a;
EXPLAIN (VERBOSE, COSTS OFF) SELECT 2.5, NULL, true, count(*) FROM gl_t;
EXPLAIN (VERBOSE, COSTS OFF) SELECT 'x'::varchar, count(*) FROM gl_t;
EXPLAIN (VERBOSE, COSTS OFF) SELECT -1, -2.5, count(*) FROM gl_t;
EXPLAIN (VERBOSE, COSTS OFF) SELECT count(*), random() FROM gl_t;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM gl_t GROUP BY id, a, s, n;
EXPLAIN (VERBOSE, COSTS OFF) SELECT *, count(*) FROM gl_t GROUP BY 1, 2, 3, 4;
EXPLAIN (VERBOSE, COSTS OFF) SELECT gl_t.*, 7 FROM gl_t GROUP BY id, a, s, n;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(*) FROM gl_t GROUP BY a HAVING count(*) > random();
EXPLAIN (VERBOSE, COSTS OFF) SELECT a FROM gl_t GROUP BY a HAVING sum(id) > random() AND count(*) > 2;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, s, count(*) FROM gl_t GROUP BY a, s HAVING max(id) > random() * 10;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(*) FROM gl_t GROUP BY a HAVING count(*) > random() OR a > 2;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a * 2, count(*) FROM gl_t GROUP BY a * 2 HAVING sum(n) > random();
EXPLAIN (COSTS OFF) SELECT a, count(*) FROM gl_t GROUP BY a HAVING sum(id) > random() ORDER BY random();
EXPLAIN (VERBOSE, COSTS OFF) SELECT a FROM gl_t GROUP BY a ORDER BY random();
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(*) FROM gl_t GROUP BY a ORDER BY count(*) * random();
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(*) FROM gl_t GROUP BY a ORDER BY random() LIMIT 2;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(*) FROM gl_t GROUP BY a ORDER BY random(), a;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(*) c FROM gl_t GROUP BY a ORDER BY c * random() DESC NULLS LAST;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(*) * random(), sum(n) FROM gl_t GROUP BY a ORDER BY a;
-- Groups filtered here: an order by grouping keys, or by an aggregate
-- under a LIMIT, still ships; any other sorts the groups the filter keeps.
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(*) FROM gl_t GROUP BY a HAVING count(*) > random() ORDER BY 2 DESC;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(*) FROM gl_t GROUP BY a HAVING count(*) > random() ORDER BY count(*) OFFSET 2;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(*) FROM gl_t GROUP BY a HAVING count(*) > random() ORDER BY count(*) LIMIT 3;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(*) FROM gl_t GROUP BY a HAVING count(*) > random() ORDER BY a;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(*) FROM gl_t GROUP BY a HAVING count(*) > random() ORDER BY a DESC;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, s, count(*) FROM gl_t GROUP BY a, s HAVING count(*) > random() ORDER BY s;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(*) FROM gl_t GROUP BY a HAVING count(*) > random() LIMIT 3;
EXPLAIN (VERBOSE, COSTS OFF) SELECT a, count(*) FROM gl_t GROUP BY a HAVING sum(id) > random() ORDER BY a LIMIT 3;
DROP SERVER gl_s CASCADE;
