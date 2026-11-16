-- A partitioned table's scan plans each partition from its own statistics
-- and indexes (set_append_rel_pathlist): an Append of each member's
-- cheapest path costed by cost_append, an ordered Append of range or
-- non-interleaved list partitions' ordered paths, or a Merge Append
-- (cost_merge_append) that sorts members whose paths lack the order.
-- Execution follows each member's path: rows come back in its order and
-- EXPLAIN ANALYZE counts what each member's scan reads. ANALYZE of the
-- partitioned table records its inherited statistics. The rows stay under
-- the forced-spill run's 256 KiB memtable.
CREATE TABLE pt750 (id int, k int, v text) PARTITION BY RANGE (id);
CREATE TABLE pt750_1 PARTITION OF pt750 FOR VALUES FROM (0) TO (1000);
CREATE TABLE pt750_2 PARTITION OF pt750 FOR VALUES FROM (1000) TO (2000);
CREATE TABLE pt750_3 PARTITION OF pt750 FOR VALUES FROM (2000) TO (3000);
INSERT INTO pt750 SELECT g, (g * 7) % 1009, 'x' || g FROM generate_series(0, 2999) g;
CREATE INDEX ON pt750 (id);
CREATE INDEX ON pt750_2 (k);
ANALYZE pt750;
SELECT relname, reltuples, relpages FROM pg_class WHERE relname LIKE 'pt750%' AND relkind IN ('r', 'p') ORDER BY 1;
SELECT tablename, attname, inherited, null_frac, avg_width, n_distinct
  FROM pg_stats WHERE tablename = 'pt750' ORDER BY 2, 3;
EXPLAIN SELECT * FROM pt750 WHERE id = 5;
EXPLAIN SELECT * FROM pt750 WHERE k = 5;
EXPLAIN SELECT * FROM pt750 ORDER BY id LIMIT 5;
EXPLAIN SELECT * FROM pt750 ORDER BY id DESC LIMIT 5;
EXPLAIN SELECT * FROM pt750 ORDER BY k LIMIT 5;
EXPLAIN SELECT * FROM pt750 WHERE id > 900 AND id < 1100;
EXPLAIN SELECT * FROM pt750 WHERE k < 20 ORDER BY k LIMIT 4;
EXPLAIN SELECT * FROM pt750;
EXPLAIN SELECT id + 1, v FROM pt750 WHERE k = 3;
EXPLAIN (VERBOSE, COSTS OFF) SELECT v FROM pt750 WHERE id BETWEEN 990 AND 1010;
\set a 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)'
:a SELECT * FROM pt750 WHERE id = 5;
:a SELECT * FROM pt750 WHERE k = 5;
:a SELECT * FROM pt750 ORDER BY id LIMIT 5;
:a SELECT * FROM pt750 ORDER BY id DESC LIMIT 5;
:a SELECT * FROM pt750 WHERE id > 900 AND id < 1100;
SELECT * FROM pt750 WHERE id = 5;
SELECT * FROM pt750 WHERE k = 5;
SELECT * FROM pt750 ORDER BY id LIMIT 5;
SELECT * FROM pt750 ORDER BY id DESC LIMIT 5;
SELECT id FROM pt750 WHERE id > 995 AND id < 1004;
SELECT * FROM pt750 WHERE k < 20 ORDER BY k LIMIT 4;
-- Equal keys of different members come out of the Merge Append's heap.
SELECT * FROM pt750 ORDER BY k LIMIT 5;
-- List partitions are ordered when no partition's values interleave.
CREATE TABLE lp750 (k int, v int, t text) PARTITION BY LIST (k);
CREATE TABLE lp750_a PARTITION OF lp750 FOR VALUES IN (1, 2);
CREATE TABLE lp750_b PARTITION OF lp750 FOR VALUES IN (3, 4);
CREATE TABLE lp750_c PARTITION OF lp750 FOR VALUES IN (5, 0);
INSERT INTO lp750 SELECT g % 6, g, 'r' || g FROM generate_series(1, 1800) g;
CREATE INDEX ON lp750 (k, v);
ANALYZE lp750;
EXPLAIN SELECT * FROM lp750 ORDER BY k LIMIT 3;
EXPLAIN SELECT * FROM lp750 WHERE k IN (1, 3) ORDER BY k, v LIMIT 3;
EXPLAIN SELECT * FROM lp750 WHERE k < 5 ORDER BY k DESC, v DESC LIMIT 3;
EXPLAIN SELECT * FROM lp750 WHERE v = 77;
EXPLAIN SELECT v + 1, t FROM lp750 WHERE k = 2 AND v < 20;
SELECT * FROM lp750 WHERE k IN (1, 3) ORDER BY k, v LIMIT 3;
SELECT * FROM lp750 WHERE k < 5 ORDER BY k DESC, v DESC LIMIT 3;
SELECT v + 1, t FROM lp750 WHERE k = 2 AND v < 20;
-- A generic plan prunes members at executor startup and runs the kept
-- members' paths.
PREPARE q750(int) AS SELECT * FROM lp750 WHERE k = $1 AND v < 100;
SET plan_cache_mode = force_generic_plan;
EXPLAIN (COSTS OFF) EXECUTE q750(3);
:a EXECUTE q750(3);
EXECUTE q750(3);
RESET plan_cache_mode;
-- A sub-partitioned table's members without index paths are one Append.
CREATE TABLE mp750 (a int, b int) PARTITION BY RANGE (a);
CREATE TABLE mp750_1 PARTITION OF mp750 FOR VALUES FROM (0) TO (100) PARTITION BY RANGE (a);
CREATE TABLE mp750_1a PARTITION OF mp750_1 FOR VALUES FROM (0) TO (50);
CREATE TABLE mp750_1b PARTITION OF mp750_1 FOR VALUES FROM (50) TO (100);
CREATE TABLE mp750_2 PARTITION OF mp750 FOR VALUES FROM (100) TO (200);
INSERT INTO mp750 SELECT g % 200, g FROM generate_series(1, 2000) g;
ANALYZE mp750;
EXPLAIN SELECT * FROM mp750 WHERE b < 100;
EXPLAIN SELECT * FROM mp750 ORDER BY b LIMIT 2;
-- Writes scan each member by its own path.
EXPLAIN UPDATE pt750 SET v = 'y' WHERE id = 5;
EXPLAIN UPDATE pt750 SET v = 'y' WHERE k = 5;
EXPLAIN DELETE FROM pt750 WHERE id < 20;
EXPLAIN (COSTS OFF) DELETE FROM pt750 WHERE k < 20;
:a UPDATE pt750 SET v = 'z' WHERE k = 7;
UPDATE pt750 SET v = 'y' WHERE k < 3 RETURNING *;
DELETE FROM pt750 WHERE id > 2990 OR k = 4 RETURNING id;
