-- Joins with a function scan or a derived table. A function in FROM whose
-- arguments read no earlier FROM item is not lateral: it is scanned once
-- and may be hashed. USING joins hash on the condition analysis
-- synthesizes, and a derived query's relation renamed for uniqueness keeps
-- that name in its keys.
CREATE TABLE j810 (a int, b int);
CREATE TABLE k810 (a int, c text);
INSERT INTO j810 SELECT g, g % 10 FROM generate_series(1, 1000) g;
INSERT INTO k810 SELECT g, 'x' || g FROM generate_series(1, 100) g;
ANALYZE j810, k810;
EXPLAIN (COSTS OFF) SELECT * FROM j810 JOIN generate_series(1, 10) g ON g = j810.a;
EXPLAIN (COSTS OFF) SELECT * FROM generate_series(1, 10) g JOIN j810 ON g = j810.a;
EXPLAIN (COSTS OFF) SELECT * FROM j810, generate_series(1, 10) g WHERE g = j810.a;
EXPLAIN (COSTS OFF) SELECT * FROM j810 JOIN unnest(ARRAY[1, 2, 3]) u(x) ON u.x = j810.a;
EXPLAIN (COSTS OFF) SELECT * FROM j810 LEFT JOIN generate_series(1, 10) g ON g = j810.a;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
  SELECT * FROM j810 JOIN generate_series(1, 10) g ON g = j810.a;
SELECT count(*), sum(g) FROM j810 JOIN generate_series(1, 10) g ON g = j810.a;
SELECT count(*) FROM j810, generate_series(1, j810.b) g WHERE g = 3;
EXPLAIN (COSTS OFF) SELECT * FROM j810 JOIN k810 USING (a);
EXPLAIN (COSTS OFF) SELECT * FROM j810 NATURAL JOIN k810;
EXPLAIN (COSTS OFF) SELECT * FROM j810 LEFT JOIN k810 USING (a);
EXPLAIN (COSTS OFF) SELECT * FROM j810 JOIN (SELECT a FROM k810 LIMIT 5) s USING (a);
EXPLAIN (COSTS OFF) SELECT j810.* FROM j810 JOIN (SELECT a FROM k810 LIMIT 5) s USING (a);
EXPLAIN (COSTS OFF) SELECT * FROM j810 LEFT JOIN (SELECT a, 1 AS one FROM k810) s USING (a);
EXPLAIN (COSTS OFF)
  SELECT * FROM j810 JOIN (SELECT a FROM j810 ORDER BY a LIMIT 5) s USING (a);
SELECT * FROM j810 JOIN (SELECT a FROM j810 ORDER BY a LIMIT 5) s USING (a) ORDER BY a;
EXPLAIN (COSTS OFF) SELECT * FROM j810 JOIN (SELECT DISTINCT b FROM j810) s USING (b);
EXPLAIN (COSTS OFF) SELECT * FROM j810 JOIN (SELECT a, count(*) FROM j810 GROUP BY a) s ON s.a = j810.a;
DROP TABLE j810, k810;
