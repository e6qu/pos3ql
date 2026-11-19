-- A limit directly above a sorted grouping stops reading its input after
-- the first row of the group past the last one it takes: a GroupAggregate
-- reads its last group's rows, a Group (no aggregates) only its first row.
-- A Group is costed by cost_group. HAVING and the projection run only on the
-- groups the limit reads. A Memoize cache over a parameterized index scan
-- keeps one entry per key.
CREATE TABLE ga941 (id int PRIMARY KEY, g int);
CREATE TABLE gb941 (id int PRIMARY KEY, a int);
INSERT INTO ga941 SELECT i, i % 50 FROM generate_series(1, 2000) i;
INSERT INTO gb941 SELECT i, i % 2000 + 1 FROM generate_series(1, 4000) i;
ANALYZE ga941, gb941;
SET enable_hashagg = off;
EXPLAIN SELECT g FROM ga941 GROUP BY g;
EXPLAIN SELECT ga941.g FROM ga941 JOIN gb941 ON gb941.a = ga941.id GROUP BY ga941.g;
EXPLAIN SELECT g FROM ga941 GROUP BY g HAVING g > 3;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT g FROM ga941 GROUP BY g;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT g FROM ga941 GROUP BY g LIMIT 2;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT g, count(*) FROM ga941 GROUP BY g LIMIT 3;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT ga941.g, count(*) FROM ga941 JOIN gb941 ON gb941.a = ga941.id GROUP BY ga941.g LIMIT 3;
SELECT g, 10 / (g - 3) FROM ga941 GROUP BY g LIMIT 3;
SELECT g, count(*) FROM ga941 GROUP BY g HAVING 10 / (g - 4) > 0 LIMIT 3;
CREATE INDEX ga941_g ON ga941 (g);
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT g, count(*) FROM ga941 GROUP BY g LIMIT 3;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT g, count(*) FROM ga941 GROUP BY g HAVING count(*) > 0 LIMIT 3 OFFSET 2;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT g, count(*) FROM ga941 GROUP BY g;
VACUUM ga941;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT g, count(*) FROM ga941 GROUP BY g LIMIT 3;
SET enable_hashjoin = off;
SET enable_mergejoin = off;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT ga941.g FROM ga941 JOIN gb941 ON gb941.a = ga941.id;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT ga941.g, count(*) FROM ga941 JOIN gb941 ON gb941.a = ga941.id GROUP BY ga941.g LIMIT 3;
SELECT ga941.g, count(*) FROM ga941 JOIN gb941 ON gb941.a = ga941.id GROUP BY ga941.g LIMIT 2;
RESET enable_hashjoin;
RESET enable_mergejoin;
RESET enable_hashagg;
DROP TABLE ga941, gb941;
