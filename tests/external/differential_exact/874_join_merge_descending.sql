-- Merge joins whose inputs sort descending or with NULLs first: the outer
-- pathkeys the query's ORDER BY gives (select_outer_pathkeys_for_merge),
-- the inner sort in the same direction (make_inner_pathkeys_for_merge),
-- mergejoinscansel's range in that direction, its NULLs counted first, and
-- execution comparing each key as its sort orders it (MJCompare, a NULL
-- first key sorted first not ending the join).
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/pathkeys.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/adt/selfuncs.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/nodeMergejoin.c
CREATE TABLE ma874 (id int, k int, v int, w text);
CREATE TABLE mb874 (id int, k int, v int, w text);
INSERT INTO ma874 SELECT g, CASE WHEN g % 11 = 0 THEN NULL ELSE g % 37 END, g % 7, 'a' || (g % 5) FROM generate_series(1, 400) g;
INSERT INTO mb874 SELECT g, CASE WHEN g % 13 = 0 THEN NULL ELSE g % 41 END, g % 5, 'b' || (g % 3) FROM generate_series(1, 300) g;
CREATE INDEX mb874_k ON mb874 (k);
ANALYZE ma874; ANALYZE mb874;
SET enable_hashjoin = off;
SET enable_nestloop = off;
EXPLAIN SELECT ma874.id, mb874.id FROM ma874 JOIN mb874 ON ma874.k = mb874.k ORDER BY ma874.k DESC;
EXPLAIN SELECT ma874.id, mb874.id FROM ma874 JOIN mb874 ON ma874.k = mb874.k ORDER BY ma874.k NULLS FIRST;
EXPLAIN SELECT ma874.id, mb874.id FROM ma874 LEFT JOIN mb874 ON ma874.k = mb874.k ORDER BY ma874.k DESC NULLS LAST, ma874.id LIMIT 30;
EXPLAIN SELECT ma874.id, mb874.id FROM ma874 FULL JOIN mb874 ON ma874.k = mb874.k ORDER BY ma874.k DESC;
SELECT ma874.id, mb874.id, ma874.k FROM ma874 JOIN mb874 ON ma874.k = mb874.k ORDER BY ma874.k DESC LIMIT 25;
SELECT ma874.id, mb874.id, ma874.k FROM ma874 LEFT JOIN mb874 ON ma874.k = mb874.k ORDER BY ma874.k NULLS FIRST LIMIT 40;
EXPLAIN SELECT ma874.id, mb874.id, ma874.k, mb874.k FROM ma874 FULL JOIN mb874 ON ma874.k = mb874.k AND ma874.v = mb874.v ORDER BY ma874.k DESC, ma874.v DESC LIMIT 40;
SELECT ma874.id, mb874.id, ma874.k, mb874.k FROM ma874 FULL JOIN mb874 ON ma874.k = mb874.k AND ma874.v = mb874.v ORDER BY ma874.k DESC, ma874.v DESC LIMIT 40;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT ma874.id, mb874.id FROM ma874 JOIN mb874 ON ma874.k = mb874.k ORDER BY ma874.k DESC;
RESET enable_hashjoin;
RESET enable_nestloop;
