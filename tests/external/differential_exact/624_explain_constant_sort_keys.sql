-- An ORDER BY key that a WHERE equality fixes to a constant is redundant in
-- the constant's equivalence class: no Sort for a table, and a foreign scan
-- ships neither that key nor, when no key is left, the LIMIT.
CREATE EXTENSION IF NOT EXISTS postgres_fdw;
CREATE SERVER ck_s FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host '127.0.0.1', port '1', dbname 'none');
CREATE USER MAPPING FOR CURRENT_USER SERVER ck_s OPTIONS (user 'nobody');
CREATE FOREIGN TABLE ck_f (id int, s smallint, note text) SERVER ck_s OPTIONS (schema_name 'remote', table_name 't');
CREATE TABLE ck_l (x int, t text, u int);
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM ck_f WHERE id = 3 ORDER BY id LIMIT 2;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM ck_f WHERE id = 3 ORDER BY id;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM ck_f WHERE id = 3 ORDER BY id, note LIMIT 2;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM ck_f WHERE note = 'n3' ORDER BY id, note LIMIT 2;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM ck_f WHERE id = 3 AND s = 3 ORDER BY s, id;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM ck_f WHERE id IN (3) ORDER BY id DESC LIMIT 1;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM ck_f WHERE id = 3 LIMIT 2;
EXPLAIN (COSTS OFF) SELECT x FROM ck_l WHERE x = 3 ORDER BY x LIMIT 2;
EXPLAIN (COSTS OFF) SELECT x, t FROM ck_l WHERE x = 3 ORDER BY x, t;
EXPLAIN (COSTS OFF) SELECT x, t FROM ck_l WHERE 3 = x AND t = 'a' ORDER BY t, x DESC;
EXPLAIN (COSTS OFF) SELECT t AS x FROM ck_l WHERE x = 3 ORDER BY x;
EXPLAIN (VERBOSE, COSTS OFF) SELECT t FROM ck_l WHERE x = 3 ORDER BY x;
DROP TABLE ck_l;
DROP SERVER ck_s CASCADE;
