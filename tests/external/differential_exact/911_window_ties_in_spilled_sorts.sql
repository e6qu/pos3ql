-- With work_mem at its minimum, the lower window clause's input sort of 1100
-- rows still fits and the upper clause's, which also holds the lower clause's
-- result, outgrows it. PostgreSQL 18 then forms quicksorted runs
-- (dumptuples) and merges them through a binary heap over the tapes' heads
-- (mergeruns), so rows tying on the upper keys come back in that heap's
-- order, and row_number, lag and running sums follow it.
SET work_mem = '64kB';
CREATE TABLE t911 (a int, b int);
INSERT INTO t911 SELECT g % 7, g FROM generate_series(1, 1100) g;
SELECT a, b, lag(b) OVER (PARTITION BY a ORDER BY b % 5), sum(b) OVER (PARTITION BY b % 3 ORDER BY a) FROM t911;
SELECT a, b, row_number() OVER (PARTITION BY b % 3 ORDER BY a), sum(b) OVER (PARTITION BY a ORDER BY b % 5) FROM t911;
SELECT a, b, rank() OVER (ORDER BY a), lag(b, 2) OVER (ORDER BY b % 4) FROM t911;
SELECT a, b, row_number() OVER (ORDER BY a % 3), row_number() OVER (ORDER BY b % 2) FROM t911;
EXPLAIN (ANALYZE, BUFFERS OFF, COSTS OFF, TIMING OFF, SUMMARY OFF) SELECT a, b, row_number() OVER (PARTITION BY a ORDER BY b % 5), sum(b) OVER (PARTITION BY b % 3 ORDER BY a) FROM t911;
