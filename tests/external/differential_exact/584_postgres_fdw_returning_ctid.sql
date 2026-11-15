-- A foreign write's RETURNING list may read the remote row's ctid, which
-- postgres_fdw asks the remote command for after the columns.
CREATE EXTENSION IF NOT EXISTS postgres_fdw;
CREATE SERVER rc_s FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host '127.0.0.1', port '1', dbname 'none');
CREATE USER MAPPING FOR CURRENT_USER SERVER rc_s OPTIONS (user 'nobody');
CREATE FOREIGN TABLE rc_t (id int, q int, note text) SERVER rc_s OPTIONS (schema_name 'remote', table_name 't');
EXPLAIN (VERBOSE, COSTS OFF) UPDATE rc_t SET q = 1 WHERE id = 3 RETURNING ctid;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE rc_t t SET q = 1 WHERE id = 3 RETURNING t.ctid::text, q;
EXPLAIN (VERBOSE, COSTS OFF) DELETE FROM rc_t WHERE id = 3 RETURNING ctid, id;
EXPLAIN (VERBOSE, COSTS OFF) INSERT INTO rc_t VALUES (1, 2, 'x') RETURNING ctid;
EXPLAIN (VERBOSE, COSTS OFF) INSERT INTO rc_t VALUES (1, 2, 'x') RETURNING note, ctid;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE rc_t SET q = 1 WHERE id = 3 AND random() < 2 RETURNING ctid;
EXPLAIN (VERBOSE, COSTS OFF) DELETE FROM rc_t WHERE id = 3 AND random() < 2 RETURNING ctid, note;
DROP SERVER rc_s CASCADE;
