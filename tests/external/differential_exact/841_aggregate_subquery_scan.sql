-- A plain aggregate reads its one derived table's physical columns
-- (use_physical_tlist refuses only grouping keys' labels), so the trivial
-- Subquery Scan between them is removed, in a subplan too.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/plan/createplan.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/plan/setrefs.c
CREATE TABLE t841 (a int, b text);
INSERT INTO t841 SELECT g, 'v' || g FROM generate_series(1, 30) g;
EXPLAIN (COSTS OFF) SELECT max(a) FROM (SELECT * FROM t841 LIMIT 5) z;
EXPLAIN (COSTS OFF) SELECT max(a), count(*) FROM (SELECT a FROM t841 ORDER BY a DESC LIMIT 5) z;
EXPLAIN (COSTS OFF) SELECT max(b) FROM (SELECT a, a + 1 AS b FROM t841 LIMIT 5) z;
EXPLAIN (COSTS OFF) SELECT max(a) FROM (SELECT DISTINCT a FROM t841) z;
EXPLAIN (COSTS OFF) SELECT count(*) FROM (SELECT a FROM t841 UNION ALL SELECT a FROM t841) z;
EXPLAIN (COSTS OFF) SELECT a, (SELECT max(a) FROM (SELECT * FROM t841 LIMIT 5) z) FROM t841 LIMIT 3;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT a, (SELECT max(a) FROM (SELECT * FROM t841 t2 WHERE t2.a > t841.a LIMIT 5) z) FROM t841 LIMIT 3;
SELECT max(a), count(*) FROM (SELECT a FROM t841 ORDER BY a DESC LIMIT 5) z;
