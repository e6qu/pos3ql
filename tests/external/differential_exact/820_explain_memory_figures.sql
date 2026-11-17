-- The memory figures PostgreSQL 18.6's EXPLAIN ANALYZE prints come from its
-- allocators' arithmetic, which pos3ql reproduces from the rows each node
-- held: a CTE Scan's tuplestore (16 kB of row pointers, doubled when a row
-- reaches the last free slot, and each row's generation chunk), a Recursive
-- Union's two tables, a hash aggregate's meta, entry and transition-value
-- contexts (hash_agg_update_metrics), a sort's SortTuple array and rows, and
-- a Materialize's tuplestore (430, 534, 536). Every table stays under the forced-spill
-- run's 256 KiB memtable.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/sort/tuplestore.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/nodeAgg.c
CREATE TABLE m820 (a int, b bigint, f float8, c text);
INSERT INTO m820 SELECT g, g, g / 7.0, 'x' || (g % 50) FROM generate_series(1, 3000) g;
ANALYZE m820;
\set a 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)'
-- A CTE's store grows its pointer array past 2048 rows.
:a WITH c AS MATERIALIZED (SELECT a FROM m820 WHERE a < 2048) SELECT * FROM c;
:a WITH c AS MATERIALIZED (SELECT a FROM m820 WHERE a < 2049) SELECT * FROM c;
:a WITH c AS MATERIALIZED (SELECT a, c FROM m820) SELECT * FROM c WHERE a < 3;
:a WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 400) SELECT count(*) FROM r;
:a WITH RECURSIVE r(n, t) AS (SELECT 1, 'a' UNION SELECT n + 1, t || 'b' FROM r WHERE n < 30) SELECT max(n) FROM r;
-- Hash tables across the bucket array's chunk and block sizes.
:a SELECT a, count(*) FROM m820 WHERE a <= 230 GROUP BY a;
:a SELECT a, count(*) FROM m820 WHERE a <= 600 GROUP BY a;
:a SELECT a, b, count(*) FROM m820 WHERE a <= 230 GROUP BY a, b;
:a SELECT a, sum(b), avg(b) FROM m820 WHERE a <= 300 GROUP BY a;
:a SELECT a, avg(f), stddev(a) FROM m820 WHERE a <= 300 GROUP BY a;
:a SELECT c FROM m820 WHERE a < 100 UNION SELECT c FROM m820 WHERE a > 2950;
-- Sorts: a datum sort of one column, bump chunks, a bounded heap.
:a SELECT a FROM m820 ORDER BY a DESC;
:a SELECT a, c FROM m820 ORDER BY c, a;
:a SELECT a, c FROM m820 ORDER BY a DESC LIMIT 10;
:a SELECT a, c FROM m820 ORDER BY a DESC LIMIT 1000;
-- A write's RETURNING rows fill its CTE's store (last: it bloats the heap).
:a WITH w AS (UPDATE m820 SET b = b WHERE a <= 300 RETURNING a, c) SELECT count(*) FROM w;
