-- An equality one of whose sides reads several relations (a.x + b.y = c.z)
-- makes that expression a member of its equivalence class
-- (process_equivalence): the join of the relations it reads computes it,
-- and it hashes or merges the join to the other side's relation, estimated
-- without statistics (DEFAULT_NUM_DISTINCT).
CREATE TABLE ga932 (x int, k int);
CREATE TABLE gb932 (y int, k int);
CREATE TABLE gc932 (z int, k int, w int);
INSERT INTO ga932 SELECT g, g % 50 FROM generate_series(1, 600) g;
INSERT INTO gb932 SELECT g % 40, g FROM generate_series(1, 300) g;
INSERT INTO gc932 SELECT g, g % 30, g % 7 FROM generate_series(1, 900) g;
ANALYZE ga932, gb932, gc932;
EXPLAIN SELECT * FROM ga932, gb932, gc932 WHERE ga932.x + gb932.y = gc932.z AND ga932.k = gb932.k;
EXPLAIN SELECT * FROM ga932 JOIN gb932 ON ga932.k = gb932.k JOIN gc932 ON ga932.x + gb932.y = gc932.z;
EXPLAIN SELECT gc932.w, count(*) FROM ga932 JOIN gb932 ON ga932.k = gb932.k
  JOIN gc932 ON gc932.z = ga932.x * 2 + gb932.y GROUP BY gc932.w;
EXPLAIN (VERBOSE, COSTS OFF) SELECT gc932.w FROM ga932 JOIN gb932 ON ga932.k = gb932.k
  JOIN gc932 ON ga932.x + gb932.y = gc932.z;
SET enable_hashjoin = off;
EXPLAIN SELECT * FROM ga932, gb932, gc932 WHERE ga932.x + gb932.y = gc932.z AND ga932.k = gb932.k;
SELECT count(*), sum(gc932.z) FROM ga932, gb932, gc932 WHERE ga932.x + gb932.y = gc932.z AND ga932.k = gb932.k;
RESET enable_hashjoin;
SELECT count(*), sum(gc932.z), max(gb932.y) FROM ga932, gb932, gc932 WHERE ga932.x + gb932.y = gc932.z AND ga932.k = gb932.k;
SELECT gc932.w, count(*) FROM ga932 JOIN gb932 ON ga932.k = gb932.k
  JOIN gc932 ON gc932.z = ga932.x * 2 + gb932.y GROUP BY gc932.w ORDER BY 1;
DROP TABLE ga932, gb932, gc932;
