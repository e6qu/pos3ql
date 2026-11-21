-- Each partition of a partitioned table is read through its own index as
-- PostgreSQL's costs choose: bitmap scans per partition for a selective
-- key, a Merge Append in index order, and a pruned single partition. A
-- partition that seeks its index counts an index scan, one read
-- sequentially a sequential scan. VACUUM of the partitioned table leaves
-- its own counts alone and marks each partition's pages all-visible.
CREATE TABLE p5 (id int, k int, v text) PARTITION BY RANGE (id);
CREATE TABLE p5a PARTITION OF p5 FOR VALUES FROM (0) TO (3000);
CREATE TABLE p5b PARTITION OF p5 FOR VALUES FROM (3000) TO (6000);
INSERT INTO p5 SELECT i, i % 600, 'v' || i FROM generate_series(0, 5999) i;
CREATE INDEX ON p5 (k);
CREATE TABLE p6 (id int) PARTITION BY RANGE (id);
CREATE TABLE p6a PARTITION OF p6 FOR VALUES FROM (0) TO (10);
INSERT INTO p6 VALUES (1), (2);
ANALYZE p5;
SELECT relname, relpages, reltuples, relallvisible FROM pg_class
 WHERE relname IN ('p5', 'p5a', 'p5b') ORDER BY relname;
EXPLAIN SELECT * FROM p5 WHERE k = 7;
SELECT * FROM p5 WHERE k = 7 ORDER BY id;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT * FROM p5 WHERE k = 7;
EXPLAIN SELECT * FROM p5 WHERE k < 3 ORDER BY k;
SELECT * FROM p5 WHERE k < 3 ORDER BY k, id LIMIT 5;
EXPLAIN SELECT * FROM p5 ORDER BY k LIMIT 3;
SELECT * FROM p5 ORDER BY k, id LIMIT 3;
EXPLAIN SELECT * FROM p5 WHERE id = 7;
EXPLAIN SELECT * FROM p5 WHERE id < 2000 AND k = 7;
SELECT * FROM p5 WHERE id < 2000 AND k = 7 ORDER BY id;
SELECT pg_stat_force_next_flush();
BEGIN;
SELECT count(*) FROM p5 WHERE k = 9;
SELECT count(*) FROM p5 WHERE v = 'v9';
SELECT count(*) FROM p6;
SELECT * FROM p6 WHERE id = 1;
SELECT relname, seq_scan, seq_tup_read, idx_scan, idx_tup_fetch
  FROM pg_stat_xact_user_tables WHERE relname ~ '^p[56]' ORDER BY relname;
COMMIT;
VACUUM p5, p6;
SELECT relname, relpages, reltuples, relallvisible FROM pg_class
 WHERE relname ~ '^p[56]' AND relkind IN ('r', 'p') ORDER BY relname;
SELECT relname, idx_scan IS NULL AS no_index, vacuum_count, analyze_count
  FROM pg_stat_user_tables WHERE relname ~ '^p[56]' ORDER BY relname;
DROP TABLE p5, p6;
