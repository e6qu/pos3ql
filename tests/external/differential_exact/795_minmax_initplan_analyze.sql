-- `min` and `max` that PostgreSQL reads from the first row of an ordered
-- index scan run as those InitPlans: under `EXPLAIN ANALYZE` each `Limit`
-- and its index scan return one row, a filtered scan counts the rows it
-- passed over, and a scalar subquery's own `ORDER BY ... LIMIT` over an
-- index stops at the limit (`preprocess_minmax_aggregates`, `ExecLimit`).
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/plan/planagg.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/nodeLimit.c
CREATE TABLE t795 (a int, b int, c text, n int);
INSERT INTO t795 SELECT CASE WHEN g % 50 = 0 THEN NULL ELSE g END, g % 10, 'x' || g, NULL FROM generate_series(1, 2000) g;
CREATE INDEX t795_a ON t795 (a);
CREATE INDEX t795_c ON t795 (c);
CREATE INDEX t795_n ON t795 (n);
VACUUM ANALYZE t795;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT min(a), max(a), min(c) FROM t795;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT max(a) FROM t795 WHERE b = 3;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT max(n) FROM t795;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT max(a) FROM t795 WHERE a < 0;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT (SELECT a FROM t795 WHERE a > 10 ORDER BY a LIMIT 1);
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT (SELECT c FROM t795 WHERE b = 4 ORDER BY a DESC LIMIT 1);
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT a FROM t795 WHERE b = 3 ORDER BY a DESC LIMIT 2 OFFSET 1;
SELECT min(a), max(a), min(c), max(c), max(n) FROM t795;
SELECT max(a) FROM t795 WHERE b = 3;
SELECT max(a) FROM t795 WHERE a < 0;
SELECT (SELECT c FROM t795 WHERE b = 4 ORDER BY a DESC LIMIT 1);
SELECT ARRAY(SELECT a FROM t795 WHERE a > 1990 ORDER BY a LIMIT 3);
SELECT (SELECT max(a) FROM t795 WHERE a < t.g) FROM generate_series(1, 3) t(g);
WITH x AS (SELECT max(a) AS m FROM t795) SELECT * FROM x;
TRUNCATE t795;
SELECT max(a), min(c) FROM t795;
