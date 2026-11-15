-- An uncorrelated scalar subquery a foreign scan's condition compares with
-- is an InitPlan whose value the remote query reads as $n::type
-- (printRemoteParam); a condition reading only InitPlans gates the scan
-- from a Result, whose scan returns its physical columns.
CREATE EXTENSION IF NOT EXISTS postgres_fdw;
CREATE SERVER ip_s FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host '127.0.0.1', port '1', dbname 'none');
CREATE USER MAPPING FOR CURRENT_USER SERVER ip_s OPTIONS (user 'nobody');
CREATE FOREIGN TABLE ip_t (id int, s smallint, b bigint, n numeric, note text, d date, q int)
  SERVER ip_s OPTIONS (schema_name 'remote', table_name 't');
CREATE TABLE ip_l (x int, t text);
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM ip_t WHERE id = (SELECT 3);
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM ip_t WHERE id > (SELECT max(id) - 3 FROM ip_t);
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM ip_t WHERE (SELECT max(x) FROM ip_l) = id OR note = (SELECT min(t) FROM ip_l);
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM ip_t WHERE id < (SELECT max(x) FROM ip_l) AND b > (SELECT min(x) FROM ip_l) * 1000000000::bigint ORDER BY id;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM ip_t WHERE id + (SELECT 1) = 4 AND (SELECT 2) < s;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM ip_t WHERE id IN ((SELECT 1), (SELECT 2));
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM ip_t WHERE d = (SELECT current_date);
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM ip_t WHERE (SELECT x FROM ip_l LIMIT 1) > 2;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM ip_t WHERE (SELECT 2) > 1 AND q > 3 ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM ip_t WHERE id = (SELECT 3) LIMIT 2;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM ip_t WHERE id = (SELECT 3) ORDER BY id LIMIT 2;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM ip_t ORDER BY id LIMIT (SELECT 2);
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, (SELECT 1) FROM ip_t ORDER BY id LIMIT 2;
EXPLAIN (VERBOSE, COSTS OFF) SELECT x FROM ip_l WHERE (SELECT 2) > 1;
EXPLAIN (VERBOSE, COSTS OFF) SELECT x FROM ip_l WHERE (SELECT 2) > 1 ORDER BY t;
EXPLAIN (VERBOSE, COSTS OFF) SELECT x FROM ip_l ORDER BY x OFFSET (SELECT 1);
EXPLAIN (VERBOSE, COSTS OFF) SELECT 1 WHERE 3 IN (SELECT id FROM ip_t);
EXPLAIN (COSTS OFF) SELECT 1 WHERE 3 IN (SELECT x FROM ip_l);
DROP TABLE ip_l;
DROP SERVER ip_s CASCADE;
