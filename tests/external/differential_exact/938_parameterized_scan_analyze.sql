-- EXPLAIN ANALYZE of a nested loop's parameterized index scan reads, per
-- outer row, the rows an index finds for the scan's own index conditions,
-- as plain execution does, rather than the whole inner relation: its rows,
-- loops and filtered rows are PostgreSQL's however large the relation is.
CREATE TABLE po938 (id int, k int);
CREATE TABLE pi938 (k int, v int, t text);
INSERT INTO po938 SELECT g, g * 37 % 2000 FROM generate_series(1, 300) g;
INSERT INTO pi938 SELECT g % 2000, g, 'v' || g FROM generate_series(1, 6000) g;
CREATE INDEX pi938_k ON pi938 (k);
ANALYZE po938, pi938;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT po938.id, pi938.v FROM po938 JOIN pi938 ON pi938.k = po938.k WHERE po938.id < 6;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT po938.id, pi938.v FROM po938 JOIN pi938 ON pi938.k = po938.k
  WHERE po938.id < 6 AND pi938.v % 2 = 0;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT count(*) FROM po938 LEFT JOIN pi938 ON pi938.k = po938.k AND pi938.v > 3000 WHERE po938.id < 6;
DROP TABLE po938, pi938;
