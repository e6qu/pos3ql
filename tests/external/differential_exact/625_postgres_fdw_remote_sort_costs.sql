-- Without a LIMIT, a foreign scan's ORDER BY ships only when postgres_fdw's
-- sorted path costs no more than a local Sort of the unordered rows: the
-- remote sort is priced at 1.2 times the scan's run cost, so a selective
-- condition leaves few rows to sort here.
CREATE EXTENSION IF NOT EXISTS postgres_fdw;
CREATE SERVER so_s FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host '127.0.0.1', port '1', dbname 'none');
CREATE USER MAPPING FOR CURRENT_USER SERVER so_s OPTIONS (user 'nobody');
CREATE FOREIGN TABLE so_t (id int, s smallint, b bigint, n numeric, f float8, note text,
  v varchar(10), c char(3), d date, ts timestamp, flag boolean, q int)
  SERVER so_s OPTIONS (schema_name 'remote', table_name 't');
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE id = 3 ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE id <> 3 ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE id > 3 ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE id > 3 AND id < 9 ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE id >= 3 AND id <= 9 AND id < 7 ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE id IN (1, 2, 3) ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE id NOT IN (1, 2, 3) ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE id IS NULL ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE id IS NOT NULL ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE upper(note) = 'X' ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE id + 1 > 3 ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE id = q ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE id < q ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE random() < 0.5 ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE note = 'a' OR id = 3 ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE id = 3 OR id = 4 OR id = 5 ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE flag ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE NOT flag ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE flag = false ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE n = 1.5 ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE d = '2024-01-05' ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE v = 'v1' ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE c = 'c1' ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE note IN ('a', 'b c', NULL) ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE b IN (1, 5000000000) ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE id % 2 = 0 ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE note || 'x' = 'n1x' ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE id = 3 AND random() < 0.5 ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE id > 3 AND random() < 0.5 ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE (id = 3 OR id = 4) AND note > 'a' ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE s = 3 ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE b > 3 AND b < 5 ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM so_t WHERE id = 3 ORDER BY id;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM so_t WHERE id = 3 ORDER BY id;
EXPLAIN (VERBOSE, COSTS OFF) SELECT v, c FROM so_t WHERE id = 3 ORDER BY c;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM so_t WHERE id > 3 ORDER BY id;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM so_t WHERE id > 3 ORDER BY id;
EXPLAIN (VERBOSE, COSTS OFF) SELECT v, c FROM so_t WHERE id > 3 ORDER BY c;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM so_t WHERE id IN (1, 2) ORDER BY id;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM so_t WHERE id IN (1, 2) ORDER BY id;
EXPLAIN (VERBOSE, COSTS OFF) SELECT v, c FROM so_t WHERE id IN (1, 2) ORDER BY c;
EXPLAIN (VERBOSE, COSTS OFF) SELECT * FROM so_t WHERE flag ORDER BY id;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id FROM so_t WHERE flag ORDER BY id;
EXPLAIN (VERBOSE, COSTS OFF) SELECT v, c FROM so_t WHERE flag ORDER BY c;
EXPLAIN (VERBOSE, COSTS OFF) SELECT id, note FROM so_t WHERE id = (SELECT 3) OR note = 'n5' ORDER BY id;
PREPARE so_p(int, text) AS SELECT id, note FROM so_t WHERE id = $1 OR note = $2 ORDER BY id;
PREPARE so_r(int) AS SELECT id, note FROM so_t WHERE id > $1 ORDER BY note;
EXPLAIN (VERBOSE, COSTS OFF) EXECUTE so_p(3, 'n5');
EXPLAIN (VERBOSE, COSTS OFF) EXECUTE so_r(3);
SET plan_cache_mode = force_generic_plan;
EXPLAIN (VERBOSE, COSTS OFF) EXECUTE so_p(3, 'n5');
EXPLAIN (VERBOSE, COSTS OFF) EXECUTE so_r(3);
RESET plan_cache_mode;
DEALLOCATE ALL;
DROP SERVER so_s CASCADE;
