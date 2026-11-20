-- A HashAggregate's Memory Usage counts its transition values as
-- PostgreSQL's transition functions allocate them: min and max keep a copy
-- of the value they return, the numeric sums grow their digit buffers,
-- string_agg doubles a 1kB buffer and array_agg an eight-element array. The
-- spill, and so the order of the groups, follows that memory.
\set origin :DBNAME
CREATE TABLE t963 (g int, t text, n numeric, i int, b bigint, iv interval, c char(6), y bytea);
-- Inserted in parts, each within a small memtable.
INSERT INTO t963
SELECT i % 50, 'v' || (i * 7 % 1000), (i * 3 % 997) / 10.0, i, i::bigint * 1000003,
  make_interval(secs => i), 'c' || (i % 13), decode(to_hex(i * 31), 'escape')
FROM generate_series(1, 1250) i;
INSERT INTO t963
SELECT i % 50, 'v' || (i * 7 % 1000), (i * 3 % 997) / 10.0, i, i::bigint * 1000003,
  make_interval(secs => i), 'c' || (i % 13), decode(to_hex(i * 31), 'escape')
FROM generate_series(1251, 2500) i;
INSERT INTO t963
SELECT i % 50, 'v' || (i * 7 % 1000), (i * 3 % 997) / 10.0, i, i::bigint * 1000003,
  make_interval(secs => i), 'c' || (i % 13), decode(to_hex(i * 31), 'escape')
FROM generate_series(2501, 3750) i;
INSERT INTO t963
SELECT i % 50, 'v' || (i * 7 % 1000), (i * 3 % 997) / 10.0, i, i::bigint * 1000003,
  make_interval(secs => i), 'c' || (i % 13), decode(to_hex(i * 31), 'escape')
FROM generate_series(3751, 5000) i;
ANALYZE t963;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT g, min(t), max(t) FROM t963 GROUP BY g;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT g, min(n), max(n), min(c), max(y) FROM t963 GROUP BY g;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT g, sum(n), avg(n) FROM t963 GROUP BY g;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT g, variance(n), stddev(b) FROM t963 GROUP BY g;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT g, sum(b), avg(iv), min(iv) FROM t963 GROUP BY g;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT g, string_agg(t, ',') FROM t963 GROUP BY g;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT g, array_agg(i), array_agg(t), array_agg(n) FROM t963 GROUP BY g;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT g, string_agg(t, ',') FILTER (WHERE i % 7 = 0), min(t || 'x') FROM t963 GROUP BY g;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT g % 5, string_agg(t, ''), min(repeat(t, 400)) FROM t963 GROUP BY 1;
SELECT g, min(t), max(t), sum(n), length(string_agg(t, ',')), array_length(array_agg(i), 1)
FROM t963 GROUP BY g;
SELECT g, avg(n), min(c), max(y) FROM t963 WHERE i < 400 GROUP BY g;
-- Under work_mem of 64kB the groups of one table spill to partitions. Each grouping runs
-- in a session of its own: a work context of an earlier aggregate may be
-- reused (928).
\c :origin
SET work_mem = '64kB';
SET enable_sort = off;
SELECT i % 2000, min(t), sum(n) FROM t963 GROUP BY 1 LIMIT 30;
\c :origin
SET work_mem = '64kB';
SET enable_sort = off;
SELECT i % 1500, array_agg(g), max(t) FROM t963 GROUP BY 1 LIMIT 30;
-- A join's grouping spills under 256kB.
\c :origin
SET work_mem = '256kB';
SET enable_sort = off;
SELECT a.i % 1800, min(b.t), sum(b.n)
FROM t963 a JOIN t963 b ON a.i = b.i GROUP BY 1 LIMIT 30;
DROP TABLE t963;
