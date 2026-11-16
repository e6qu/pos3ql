-- An inheritance tree's members are planned like a partitioned table's:
-- the parent and each child by its own statistics and indexes, under an
-- Append or a Merge Append; a Result above the Append computes a select
-- list the members do not return. ANALYZE of the parent records its own
-- and its inherited statistics. The rows stay under the forced-spill run's
-- 256 KiB memtable.
CREATE TABLE ip751 (id int, k int);
CREATE TABLE ic751 (x int) INHERITS (ip751);
CREATE TABLE id751 (y text) INHERITS (ip751);
INSERT INTO ip751 SELECT g, g FROM generate_series(1, 100) g;
INSERT INTO ic751 SELECT g, g % 50, g FROM generate_series(1, 400) g;
INSERT INTO id751 SELECT g, g % 7, 'y' || g FROM generate_series(1, 300) g;
CREATE INDEX ON ic751 (k);
CREATE INDEX ON id751 (id);
ANALYZE ip751;
SELECT relname, reltuples, relpages FROM pg_class WHERE relname IN ('ip751', 'ic751', 'id751') ORDER BY 1;
SELECT tablename, attname, inherited, null_frac, avg_width, n_distinct
  FROM pg_stats WHERE tablename = 'ip751' ORDER BY 2, 3;
EXPLAIN SELECT * FROM ip751 WHERE k = 5;
EXPLAIN SELECT * FROM ip751 ORDER BY k;
EXPLAIN SELECT * FROM ip751 ORDER BY k LIMIT 1;
EXPLAIN SELECT * FROM ip751 WHERE id < 3;
EXPLAIN SELECT id * 2 FROM ip751 WHERE k = 5;
EXPLAIN SELECT * FROM ONLY ip751 WHERE k = 5;
\set a 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)'
:a SELECT * FROM ip751 WHERE k = 5;
:a SELECT * FROM ip751 WHERE id < 3;
SELECT * FROM ip751 WHERE k = 5;
SELECT * FROM ip751 WHERE id < 3;
SELECT id * 2 FROM ip751 WHERE k = 5;
SELECT tableoid::regclass, id FROM ip751 ORDER BY id DESC, k LIMIT 4;
EXPLAIN UPDATE ip751 SET k = k + 1 WHERE k = 5;
EXPLAIN UPDATE ip751 SET k = id WHERE k = 5;
EXPLAIN DELETE FROM ip751 WHERE k < 3;
UPDATE ip751 SET k = k + 1000 WHERE k < 2 RETURNING tableoid::regclass, *;
DELETE FROM ip751 WHERE id < 3 RETURNING tableoid::regclass, id;
