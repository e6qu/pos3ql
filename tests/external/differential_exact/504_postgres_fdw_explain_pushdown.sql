-- postgres_fdw's plans need no connection: EXPLAIN shows the remote query
-- each foreign scan, UPDATE, DELETE and INSERT would send.
CREATE EXTENSION IF NOT EXISTS postgres_fdw;
CREATE SERVER ep_s FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host '127.0.0.1', port '1', dbname 'none');
CREATE USER MAPPING FOR CURRENT_USER SERVER ep_s OPTIONS (user 'nobody');
CREATE FOREIGN TABLE ep_t (id int, q int, note text, d date, flag boolean)
  SERVER ep_s OPTIONS (schema_name 'remote', table_name 't', batch_size '4');
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM ep_t;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM ep_t WHERE q > 50 AND note <> 'x' AND id = 3 ORDER BY q DESC LIMIT 3;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM ep_t WHERE note LIKE 'n1%' OR id IN (1, 2) ORDER BY note NULLS FIRST LIMIT 2 OFFSET 1;
EXPLAIN (VERBOSE, COSTS OFF) SELECT note FROM ep_t WHERE random() > 0.5 AND d < '2024-01-05' AND flag;
EXPLAIN (VERBOSE, COSTS OFF) SELECT q + 1, lower(note) FROM ep_t WHERE upper(note) = 'N1';
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM ep_t ORDER BY q, note DESC;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM ep_t WHERE random() < 2 ORDER BY q LIMIT 1;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM ep_t WHERE id > 3 FOR UPDATE;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE ep_t SET q = q + 1, note = 'y' WHERE id = 3;
EXPLAIN (VERBOSE, COSTS OFF) UPDATE ep_t SET q = q + 1 WHERE id = 3 AND random() < 2;
EXPLAIN (VERBOSE, COSTS OFF) DELETE FROM ep_t WHERE flag IS NOT TRUE OR id = 4;
EXPLAIN (VERBOSE, COSTS OFF) DELETE FROM ep_t WHERE id = 4 AND random() < 2;
EXPLAIN (VERBOSE, COSTS OFF) INSERT INTO ep_t VALUES (1, 2, 'x', NULL, true);
EXPLAIN (FORMAT JSON, VERBOSE, COSTS OFF) SELECT id FROM ep_t WHERE id = 1;
DROP SERVER ep_s CASCADE;
