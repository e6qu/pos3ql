-- A generic plan keeps every partition its constants leave and prunes the
-- rest at executor startup with the arguments EXPLAIN EXECUTE passes,
-- printing how many it removed (`Subplans Removed`) and naming the
-- partitions left; a custom plan prunes with the arguments as constants.
-- Stable functions prune at startup too; a plan keeping one partition
-- scans it without an Append and prunes nothing more.
CREATE TABLE pt602 (id int, v int) PARTITION BY RANGE (id);
CREATE TABLE pt602_1 PARTITION OF pt602 FOR VALUES FROM (1) TO (10);
CREATE TABLE pt602_2 PARTITION OF pt602 FOR VALUES FROM (10) TO (20);
CREATE TABLE pt602_3 PARTITION OF pt602 FOR VALUES FROM (20) TO (30);
INSERT INTO pt602 SELECT g, g FROM generate_series(1, 29) g;
PREPARE c602(int) AS SELECT * FROM pt602 WHERE id = $1;
EXPLAIN (COSTS OFF) EXECUTE c602(3);
EXPLAIN (COSTS OFF) EXECUTE c602(300);
SET plan_cache_mode = force_generic_plan;
EXPLAIN (COSTS OFF) EXECUTE c602(3);
EXPLAIN (COSTS OFF) EXECUTE c602(15);
EXPLAIN (COSTS OFF) EXECUTE c602(300);
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE c602(15);
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) EXECUTE c602(300);
PREPARE r602(int) AS SELECT * FROM pt602 WHERE id < $1;
EXPLAIN (COSTS OFF) EXECUTE r602(15);
PREPARE b602(int) AS SELECT * FROM pt602 WHERE id = $1 AND id < 12;
EXPLAIN (COSTS OFF) EXECUTE b602(5);
EXPLAIN (COSTS OFF) EXECUTE b602(15);
PREPARE o602(int) AS SELECT * FROM pt602 WHERE id = $1 AND id < 5;
EXPLAIN (COSTS OFF) EXECUTE o602(3);
PREPARE a602(int) AS SELECT count(*) FROM pt602 WHERE id = $1;
EXPLAIN (COSTS OFF) EXECUTE a602(5);
PREPARE n602(int) AS SELECT * FROM pt602 WHERE v = $1;
EXPLAIN (COSTS OFF) EXECUTE n602(5);
PREPARE s602(int) AS SELECT * FROM pt602 WHERE id = $1 ORDER BY v;
EXPLAIN (COSTS OFF) EXECUTE s602(5);
PREPARE u602(int) AS UPDATE pt602 SET v = 1 WHERE id = $1;
EXPLAIN (COSTS OFF) EXECUTE u602(5);
PREPARE d602(int) AS DELETE FROM pt602 WHERE id > $1;
EXPLAIN (COSTS OFF) EXECUTE d602(25);
RESET plan_cache_mode;
EXPLAIN (COSTS OFF) SELECT * FROM pt602 WHERE id = 1 + (current_setting('work_mem') = 'x')::int;
EXPLAIN (COSTS OFF) SELECT * FROM pt602 WHERE id = (current_setting('work_mem') = 'x')::int + 100;
EXPLAIN (COSTS OFF) SELECT * FROM pt602 WHERE id = (SELECT 5);
