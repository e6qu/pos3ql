-- postgres_fdw sends the order the plan reads the rows in: ORDER BY keys
-- that are shippable expressions, and the order DISTINCT reads them in,
-- which leaves a Unique node over the scan.
CREATE EXTENSION IF NOT EXISTS postgres_fdw;
CREATE SERVER ro_s FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host '127.0.0.1', port '1', dbname 'none');
CREATE USER MAPPING FOR CURRENT_USER SERVER ro_s OPTIONS (user 'nobody');
CREATE FOREIGN TABLE ro_t (id int, a int, s text) SERVER ro_s OPTIONS (schema_name 'remote', table_name 't');
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM ro_t ORDER BY id + 1;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM ro_t ORDER BY a * 2 DESC, id LIMIT 3;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM ro_t ORDER BY upper(s);
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, a + id AS z FROM ro_t ORDER BY z;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM ro_t ORDER BY s || 'x';
EXPLAIN (VERBOSE, COSTS OFF) SELECT DISTINCT a FROM ro_t;
EXPLAIN (VERBOSE, COSTS OFF) SELECT DISTINCT a FROM ro_t ORDER BY a DESC;
EXPLAIN (VERBOSE, COSTS OFF) SELECT DISTINCT a, s FROM ro_t;
EXPLAIN (VERBOSE, COSTS OFF) SELECT DISTINCT s, a FROM ro_t ORDER BY a;
EXPLAIN (VERBOSE, COSTS OFF) SELECT DISTINCT ON (a) a, s FROM ro_t ORDER BY a, s;
EXPLAIN (VERBOSE, COSTS OFF) SELECT DISTINCT a FROM ro_t LIMIT 2;
DROP SERVER ro_s CASCADE;
