-- The lower clause sorts one column as datums and fits work_mem; the upper
-- clause's tuples (the key and five lower results) outgrow it, so PostgreSQL 18
-- writes many quicksorted runs, deals them over six tapes (the minimum merge
-- order) and merges in several passes. Rows tying on the key follow that
-- heap's order, which changes with work_mem.
CREATE TABLE t913 (a int, b int);
INSERT INTO t913 SELECT g % 7, g FROM generate_series(1, 7500) g;
SET work_mem = '192kB';
SELECT a, rank() OVER (ORDER BY a) r1, dense_rank() OVER (ORDER BY a) r2, count(*) OVER (ORDER BY a) r3, percent_rank() OVER (ORDER BY a) r4, cume_dist() OVER (ORDER BY a) r5, row_number() OVER (ORDER BY a DESC) n FROM t913 OFFSET 100 LIMIT 25;
SELECT a, rank() OVER (ORDER BY a) r1, dense_rank() OVER (ORDER BY a) r2, count(*) OVER (ORDER BY a) r3, percent_rank() OVER (ORDER BY a) r4, cume_dist() OVER (ORDER BY a) r5, row_number() OVER (ORDER BY a DESC) n FROM t913 OFFSET 3700 LIMIT 25;
SET work_mem = '256kB';
SELECT a, rank() OVER (ORDER BY a) r1, dense_rank() OVER (ORDER BY a) r2, count(*) OVER (ORDER BY a) r3, percent_rank() OVER (ORDER BY a) r4, cume_dist() OVER (ORDER BY a) r5, row_number() OVER (ORDER BY a DESC) n FROM t913 OFFSET 100 LIMIT 25;
SELECT a, rank() OVER (ORDER BY a) r1, dense_rank() OVER (ORDER BY a) r2, count(*) OVER (ORDER BY a) r3, percent_rank() OVER (ORDER BY a) r4, cume_dist() OVER (ORDER BY a) r5, row_number() OVER (ORDER BY a DESC) n FROM t913 OFFSET 3700 LIMIT 25;
SET work_mem = '384kB';
SELECT a, rank() OVER (ORDER BY a) r1, dense_rank() OVER (ORDER BY a) r2, count(*) OVER (ORDER BY a) r3, percent_rank() OVER (ORDER BY a) r4, cume_dist() OVER (ORDER BY a) r5, row_number() OVER (ORDER BY a DESC) n FROM t913 OFFSET 100 LIMIT 25;
SELECT a, rank() OVER (ORDER BY a) r1, dense_rank() OVER (ORDER BY a) r2, count(*) OVER (ORDER BY a) r3, percent_rank() OVER (ORDER BY a) r4, cume_dist() OVER (ORDER BY a) r5, row_number() OVER (ORDER BY a DESC) n FROM t913 OFFSET 3700 LIMIT 25;
