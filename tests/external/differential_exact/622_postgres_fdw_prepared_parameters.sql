-- A prepared statement's parameter in a foreign scan's condition ships: a
-- custom plan sends its value as a constant, a generic plan sends $n::type
-- (printRemoteParam). A parameter LIMIT stays local, and a condition
-- reading only parameters gates the scan.
CREATE EXTENSION IF NOT EXISTS postgres_fdw;
CREATE SERVER pp_s FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host '127.0.0.1', port '1', dbname 'none');
CREATE USER MAPPING FOR CURRENT_USER SERVER pp_s OPTIONS (user 'nobody');
CREATE FOREIGN TABLE pp_t (id int, b bigint, n numeric, note text, d date, ts timestamp)
  SERVER pp_s OPTIONS (schema_name 'remote', table_name 't');
PREPARE pp1(int) AS SELECT id FROM pp_t WHERE id = $1;
PREPARE pp2(text, bigint, int) AS SELECT id FROM pp_t WHERE note = $1 OR b = $2 ORDER BY id LIMIT $3;
PREPARE pp3(int) AS SELECT id FROM pp_t WHERE $1 > 0 AND id < 3;
PREPARE pp4(date, timestamp) AS SELECT id FROM pp_t WHERE d = $1 AND ts < $2;
PREPARE pp5(int) AS SELECT id FROM pp_t WHERE id = $1 + 1;
PREPARE pp6(int, int) AS SELECT id FROM pp_t WHERE id IN ($1, $2);
PREPARE pp7(numeric) AS SELECT id FROM pp_t WHERE n > $1 ORDER BY id LIMIT 2;
EXPLAIN (VERBOSE, COSTS OFF) EXECUTE pp1(3);
EXPLAIN (VERBOSE, COSTS OFF) EXECUTE pp2('n2', 5000000000, 2);
EXPLAIN (VERBOSE, COSTS OFF) EXECUTE pp4('2024-01-05', '2024-01-01 10:00');
EXPLAIN (VERBOSE, COSTS OFF) EXECUTE pp5(2);
EXPLAIN (VERBOSE, COSTS OFF) EXECUTE pp6(1, 2);
EXPLAIN (VERBOSE, COSTS OFF) EXECUTE pp7(1.5);
SET plan_cache_mode = force_generic_plan;
EXPLAIN (VERBOSE, COSTS OFF) EXECUTE pp1(3);
EXPLAIN (VERBOSE, COSTS OFF) EXECUTE pp2('n2', 5000000000, 2);
EXPLAIN (VERBOSE, COSTS OFF) EXECUTE pp3(1);
EXPLAIN (VERBOSE, COSTS OFF) EXECUTE pp4('2024-01-05', '2024-01-01 10:00');
EXPLAIN (VERBOSE, COSTS OFF) EXECUTE pp5(2);
EXPLAIN (VERBOSE, COSTS OFF) EXECUTE pp6(1, 2);
EXPLAIN (VERBOSE, COSTS OFF) EXECUTE pp7(1.5);
RESET plan_cache_mode;
DEALLOCATE ALL;
DROP SERVER pp_s CASCADE;
