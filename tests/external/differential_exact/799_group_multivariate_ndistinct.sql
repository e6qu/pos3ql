-- GROUP BY estimates its groups as PostgreSQL's `estimate_num_groups` does,
-- reading an ndistinct statistics object's count for the columns it covers
-- (`estimate_multivariate_ndistinct`), which the hashed and sorted
-- strategies' costs and the plan's row estimate follow.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/adt/selfuncs.c
CREATE TABLE t799 (a int, b int, c int);
INSERT INTO t799 SELECT g % 50, g % 50, g % 7 FROM generate_series(1, 5000) g;
ANALYZE t799;
EXPLAIN SELECT a, b, count(*) FROM t799 GROUP BY a, b;
CREATE STATISTICS t799_ab (ndistinct) ON a, b FROM t799;
ANALYZE t799;
EXPLAIN SELECT a, b, count(*) FROM t799 GROUP BY a, b;
EXPLAIN SELECT a, b, c, count(*) FROM t799 GROUP BY a, b, c ORDER BY a, b, c;
SET enable_hashagg = off;
EXPLAIN SELECT a, b, count(*) FROM t799 GROUP BY a, b;
RESET enable_hashagg;
SELECT a, b, count(*) FROM t799 WHERE a < 3 GROUP BY a, b ORDER BY a, b;
