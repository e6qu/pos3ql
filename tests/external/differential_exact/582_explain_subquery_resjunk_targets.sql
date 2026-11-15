-- A grouped foreign scan read through a view or subquery qualifies its
-- columns, as the range table holds more than its relation; a subquery
-- whose plan returns keys it does not select keeps its Subquery Scan.
CREATE EXTENSION IF NOT EXISTS postgres_fdw;
CREATE SERVER qv_s FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host '127.0.0.1', port '1', dbname 'none');
CREATE USER MAPPING FOR CURRENT_USER SERVER qv_s OPTIONS (user 'nobody');
CREATE FOREIGN TABLE qv_t (id int, a int, s text, n numeric) SERVER qv_s OPTIONS (schema_name 'remote', table_name 't');
CREATE VIEW qv_v AS SELECT a, count(*) c FROM qv_t GROUP BY a;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM (SELECT a, count(*) FROM qv_t GROUP BY a) s;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM qv_v;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM qv_v WHERE a > 1;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM (SELECT a, count(*) * random() FROM qv_t GROUP BY a HAVING sum(id) > random()) s;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM (SELECT a, sum(n) FROM qv_t GROUP BY a ORDER BY random()) s;
EXPLAIN (VERBOSE, COSTS OFF) WITH w AS (SELECT a, count(*) FROM qv_t GROUP BY a) SELECT * FROM w;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM (SELECT t.a, max(t.s) FROM qv_t t GROUP BY t.a) s;
CREATE TABLE vk (id int, a int, s text);
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM (SELECT a, sum(id) FROM vk GROUP BY a ORDER BY random()) s;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM (SELECT a FROM vk ORDER BY id) s;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM (SELECT sum(id) FROM vk GROUP BY a) s;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM (SELECT a, sum(id) FROM vk GROUP BY a ORDER BY a) s;
CREATE TABLE vj (id int, a int, s text);
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM (SELECT a, rank() OVER (ORDER BY id) FROM vj) s;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM (SELECT id, rank() OVER (ORDER BY id) FROM vj) s;
DROP TABLE vk, vj;
DROP SERVER qv_s CASCADE;
