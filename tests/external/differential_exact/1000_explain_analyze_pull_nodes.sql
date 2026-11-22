-- EXPLAIN ANALYZE counts of joins the general executor reads through the
-- pull nodes. A nested loop's Materialize reads its input only as far as
-- an anti join's loops fetch it, the input's own filter counted; a join
-- filter's removed rows count on the join; a Memoize reports its hits and
-- misses; a merge join counts the rows each sort returned. A LATERAL
-- subquery runs for each outer row.
CREATE TABLE ao (id int, k int, pad text);
CREATE TABLE ai (k int, v int, w text);
INSERT INTO ao SELECT g, g % 300, repeat('o', 30) FROM generate_series(1, 1500) g;
INSERT INTO ai SELECT g % 500, g % 7, 'w' || g || repeat('i', 100) FROM generate_series(1, 1000) g;
CREATE INDEX ai_k ON ai (k);
ANALYZE ao, ai;
SET enable_hashjoin = off;
SET enable_mergejoin = off;
SET enable_memoize = off;
SET enable_indexscan = off;
SET enable_bitmapscan = off;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT count(*) FROM ao WHERE NOT EXISTS (SELECT FROM ai WHERE ai.k = ao.k AND ai.v < 3);
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT ao.id FROM ao JOIN ai ON ai.k = ao.k AND ai.v + ao.id % 3 = 4 ORDER BY 1 LIMIT 5;
RESET enable_indexscan;
RESET enable_memoize;
SET work_mem = '64kB';
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT count(*) FROM ao JOIN ai ON ai.k = ao.k;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT count(*) FROM ao JOIN ai ON ai.k = ao.id % 450;
RESET work_mem;
SET enable_nestloop = off;
RESET enable_mergejoin;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT count(*) FROM ai a JOIN ai b ON a.k = b.k AND a.v < b.v;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT a.k FROM ai a LEFT JOIN ao b ON b.k = a.k WHERE a.k < 20 ORDER BY a.k LIMIT 3;
RESET ALL;
SELECT ao.id, s.n FROM ao, LATERAL (SELECT count(*) AS n FROM ai WHERE ai.k = ao.k AND ai.v > ao.id % 5) s WHERE ao.id < 6 ORDER BY 1;
DROP TABLE ao, ai;
