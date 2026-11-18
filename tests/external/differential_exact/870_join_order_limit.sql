-- The query's ordering, LIMIT and output expressions over a join PostgreSQL's
-- join search plans: the ORDER BY's pathkeys (each column's equivalence
-- class, a new one for a column no class holds) make index and merge join
-- orders useful (truncate_useless_pathkeys, select_outer_pathkeys_for_merge),
-- a LIMIT makes every relation consider startup cost, and the joined
-- relation's paths are sorted, incrementally sorted or kept presorted
-- (create_ordered_paths) under a Limit (create_limit_path); the cheapest
-- final path runs. The select list's expressions cost each joined row
-- (create_projection_path). A Sort sorts as tuplesort does: quicksort, or a
-- bounded heap past twice a limit's rows, and an Incremental Sort sorts its
-- input's groups in batches (ExecIncrementalSort), so rows with equal keys
-- come back in PostgreSQL's order.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/plan/planner.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/pathkeys.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/sort/tuplesort.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/nodeIncrementalSort.c
CREATE TABLE oa870 (id int, k int, v int, w text);
CREATE TABLE ob870 (id int, k int, v int, w text);
CREATE TABLE oc870 (id int, k int, v int, w text);
INSERT INTO oa870 SELECT g, g % 50, (g * 7) % 13, 'a' || (g % 9) FROM generate_series(1, 800) g;
INSERT INTO ob870 SELECT g, g % 300, (g * 7) % 13, 'b' || (g % 9) FROM generate_series(1, 600) g;
INSERT INTO oc870 SELECT g, CASE WHEN g % 7 = 3 THEN NULL ELSE g % 20 END, (g * 7) % 13, 'c' || (g % 9) FROM generate_series(1, 100) g;
CREATE INDEX ob870_k ON ob870 (k);
CREATE UNIQUE INDEX oa870_id ON oa870 (id);
ANALYZE oa870; ANALYZE ob870; ANALYZE oc870;
EXPLAIN SELECT oa870.id, ob870.id FROM oa870 JOIN ob870 ON oa870.k = ob870.k ORDER BY oa870.k;
SELECT oa870.id, ob870.id FROM oa870 JOIN ob870 ON oa870.k = ob870.k ORDER BY oa870.k LIMIT 30;
EXPLAIN SELECT oa870.id, ob870.id FROM oa870 JOIN ob870 ON oa870.k = ob870.k ORDER BY oa870.id LIMIT 5;
SELECT oa870.id, ob870.id FROM oa870 JOIN ob870 ON oa870.k = ob870.k ORDER BY oa870.id LIMIT 5;
EXPLAIN SELECT oa870.id, ob870.id FROM oa870 JOIN ob870 ON oa870.k = ob870.k ORDER BY ob870.k LIMIT 5;
SELECT oa870.id, ob870.id FROM oa870 JOIN ob870 ON oa870.k = ob870.k ORDER BY ob870.k LIMIT 5;
EXPLAIN SELECT oa870.id, ob870.v FROM oa870 JOIN ob870 ON oa870.id = ob870.k ORDER BY ob870.v, oa870.id;
SELECT oa870.id, ob870.v FROM oa870 JOIN ob870 ON oa870.id = ob870.k ORDER BY ob870.v, oa870.id;
EXPLAIN SELECT oa870.id, oc870.id FROM oa870 LEFT JOIN oc870 ON oa870.v = oc870.v ORDER BY oa870.id DESC LIMIT 7;
SELECT oa870.id, oc870.id FROM oa870 LEFT JOIN oc870 ON oa870.v = oc870.v ORDER BY oa870.id DESC LIMIT 7;
EXPLAIN SELECT oa870.w, ob870.w FROM oa870 JOIN ob870 ON oa870.k = ob870.k ORDER BY ob870.v;
SELECT oa870.w, ob870.w FROM oa870 JOIN ob870 ON oa870.k = ob870.k ORDER BY ob870.v;
EXPLAIN SELECT oa870.w, ob870.w FROM oa870 JOIN ob870 ON oa870.k = ob870.k ORDER BY ob870.v LIMIT 40;
SELECT oa870.w, ob870.w FROM oa870 JOIN ob870 ON oa870.k = ob870.k ORDER BY ob870.v LIMIT 40;
SELECT oa870.w, ob870.w FROM oa870 JOIN ob870 ON oa870.k = ob870.k ORDER BY ob870.v LIMIT 10 OFFSET 25;
EXPLAIN SELECT oa870.id + 1, ob870.id FROM oa870 JOIN ob870 ON oa870.k = ob870.k;
EXPLAIN SELECT oa870.id, oc870.k FROM oa870 JOIN oc870 ON oa870.v = oc870.v ORDER BY oc870.k NULLS FIRST, oa870.w;
SELECT oa870.id, oc870.k FROM oa870 JOIN oc870 ON oa870.v = oc870.v ORDER BY oc870.k NULLS FIRST, oa870.w LIMIT 60;
EXPLAIN SELECT oa870.id, ob870.w, oc870.id FROM oa870 JOIN ob870 ON oa870.k = ob870.k JOIN oc870 ON ob870.v = oc870.v ORDER BY oa870.k, oc870.id LIMIT 12;
SELECT oa870.id, ob870.w, oc870.id FROM oa870 JOIN ob870 ON oa870.k = ob870.k JOIN oc870 ON ob870.v = oc870.v ORDER BY oa870.k, oc870.id LIMIT 12;
EXPLAIN SELECT oa870.id, ob870.id FROM oa870 JOIN ob870 ON oa870.k = ob870.k ORDER BY oa870.k, ob870.w;
SELECT oa870.id, ob870.id FROM oa870 JOIN ob870 ON oa870.k = ob870.k ORDER BY oa870.k, ob870.w;
SELECT oa870.id, ob870.id FROM oa870 JOIN ob870 ON oa870.k = ob870.k ORDER BY oa870.k, ob870.w LIMIT 50;
EXPLAIN SELECT oa870.id, ob870.id FROM oa870 JOIN ob870 ON oa870.k = ob870.k WHERE oa870.k = 7 ORDER BY ob870.k, oa870.id;
SELECT oa870.id * 2, ob870.id FROM oa870 JOIN ob870 ON oa870.k = ob870.k WHERE oa870.k = 7 ORDER BY ob870.k, oa870.id;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT oa870.id, ob870.id FROM oa870 JOIN ob870 ON oa870.k = ob870.k ORDER BY oa870.id LIMIT 5;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT oa870.w, ob870.w FROM oa870 JOIN ob870 ON oa870.k = ob870.k ORDER BY ob870.v LIMIT 40;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT oa870.id, ob870.v FROM oa870 JOIN ob870 ON oa870.id = ob870.k ORDER BY ob870.v, oa870.id;
