-- A LATERAL subquery PostgreSQL keeps as a subquery is a base relation of
-- the join search: its plan's costs, a nested loop reading it per outer
-- row, and a Memoize cache keyed by the outer columns it reads (binary
-- mode), whose hits, misses and memory EXPLAIN ANALYZE counts.
CREATE TABLE lo (a int, x int);
INSERT INTO lo SELECT g % 10, g FROM generate_series(1, 1000) g;
CREATE TABLE li (b int, c int, d text);
INSERT INTO li SELECT g % 100, g, 'd' || g FROM generate_series(1, 5000) g;
CREATE TABLE l3 (k int, v int);
INSERT INTO l3 SELECT g, g FROM generate_series(1, 50) g;
ANALYZE lo, li, l3;
EXPLAIN SELECT * FROM lo, LATERAL (SELECT count(*) FROM li WHERE li.b = lo.a) s;
EXPLAIN SELECT * FROM lo, LATERAL (SELECT c FROM li WHERE li.b = lo.a ORDER BY c LIMIT 2) s;
EXPLAIN SELECT * FROM lo CROSS JOIN LATERAL (SELECT max(c), min(d) FROM li WHERE li.b = lo.a) s;
EXPLAIN SELECT * FROM lo, LATERAL (SELECT count(*), max(c) FROM li WHERE li.b = lo.a AND li.c > lo.x) s;
EXPLAIN SELECT * FROM lo JOIN l3 ON l3.k = lo.x, LATERAL (SELECT count(*) FROM li WHERE li.b = lo.a) s;
EXPLAIN SELECT * FROM l3, lo, LATERAL (SELECT count(*) FROM li WHERE li.b = lo.a) s WHERE l3.k = lo.x;
EXPLAIN SELECT * FROM lo, LATERAL (SELECT count(*) FROM li WHERE li.b = lo.a) s WHERE lo.x < 3;
EXPLAIN SELECT * FROM lo, LATERAL (SELECT c FROM li WHERE li.b = lo.x LIMIT 1) s;
EXPLAIN SELECT lo.x, s.* FROM lo, LATERAL (SELECT count(*) FROM li WHERE li.b = lo.a) s;
EXPLAIN (COSTS OFF) SELECT * FROM lo, LATERAL (SELECT count(*) FROM li WHERE li.b = lo.a) s;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
  SELECT * FROM lo, LATERAL (SELECT count(*) FROM li WHERE li.b = lo.a) s;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
  SELECT * FROM lo, LATERAL (SELECT c FROM li WHERE li.b = lo.a ORDER BY c LIMIT 2) s;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
  SELECT * FROM lo JOIN l3 ON l3.k = lo.x, LATERAL (SELECT count(*) FROM li WHERE li.b = lo.a) s;
SELECT * FROM lo, LATERAL (SELECT count(*) FROM li WHERE li.b = lo.a) s WHERE lo.x < 13;
SELECT * FROM lo, LATERAL (SELECT c FROM li WHERE li.b = lo.a ORDER BY c LIMIT 2) s
  WHERE lo.x BETWEEN 5 AND 8;
SELECT count(*), sum(s.count) FROM lo, LATERAL (SELECT count(*) FROM li WHERE li.b = lo.a) s
  WHERE lo.x <= 30;
SET enable_memoize = off;
EXPLAIN SELECT * FROM lo, LATERAL (SELECT count(*) FROM li WHERE li.b = lo.a) s;
EXPLAIN SELECT * FROM lo, LATERAL (SELECT c FROM li WHERE li.b = lo.a ORDER BY c LIMIT 2) s;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
  SELECT * FROM lo, LATERAL (SELECT count(*) FROM li WHERE li.b = lo.a) s WHERE lo.x < 4;
RESET enable_memoize;
-- A correlated scalar subquery in a select list costs its subplan per row.
EXPLAIN SELECT a, (SELECT count(*) FROM li WHERE li.b = lo.a) FROM lo;
EXPLAIN SELECT a, (SELECT c FROM li WHERE li.b = lo.a LIMIT 1) FROM lo;
