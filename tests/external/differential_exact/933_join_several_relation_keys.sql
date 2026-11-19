-- An expression of several relations (ga933.x + gb933.y) in an equivalence
-- class parameterizes an index scan of the class's other member, which a
-- nested loop over their join runs once per joined row
-- (generate_implied_equalities_for_column); with a constant in the class it
-- becomes a join filter of their join (generate_base_implied_equalities_const);
-- and ORDER BY, window and ordered-aggregate keys sort by it.
CREATE TABLE ga933 (x int, k int);
CREATE TABLE gb933 (y int, k int);
CREATE TABLE gc933 (z int, k int, w int);
INSERT INTO ga933 SELECT g, g % 50 FROM generate_series(1, 600) g;
INSERT INTO gb933 SELECT g % 40, g FROM generate_series(1, 300) g;
INSERT INTO gc933 SELECT g, g % 30, g % 7 FROM generate_series(1, 5000) g;
CREATE INDEX gc933_z ON gc933 (z);
ANALYZE ga933, gb933, gc933;
EXPLAIN SELECT * FROM ga933, gb933, gc933 WHERE ga933.x + gb933.y = gc933.z AND ga933.k = gb933.k;
EXPLAIN SELECT * FROM ga933, gb933, gc933
  WHERE ga933.x + gb933.y = gc933.z AND ga933.k = gb933.k AND gc933.z = 10;
EXPLAIN SELECT * FROM ga933, gb933, gc933
  WHERE ga933.x + gb933.y = gc933.z AND ga933.k = gb933.k ORDER BY ga933.x + gb933.y LIMIT 5;
EXPLAIN SELECT sum(ga933.x) OVER (ORDER BY ga933.x + gb933.y) FROM ga933, gb933
  WHERE ga933.k = gb933.k LIMIT 3;
EXPLAIN SELECT sum(ga933.x) OVER (PARTITION BY ga933.x + gb933.y) FROM ga933, gb933
  WHERE ga933.k = gb933.k LIMIT 3;
EXPLAIN SELECT string_agg(ga933.x::text, ',' ORDER BY ga933.x + gb933.y) FROM ga933, gb933
  WHERE ga933.k = gb933.k;
SELECT count(*), sum(gc933.w) FROM ga933, gb933, gc933
  WHERE ga933.x + gb933.y = gc933.z AND ga933.k = gb933.k;
SELECT count(*), sum(gc933.w) FROM ga933, gb933, gc933
  WHERE ga933.x + gb933.y = gc933.z AND ga933.k = gb933.k AND gc933.z = 10;
SELECT sum(ga933.x) OVER (ORDER BY ga933.x + gb933.y, ga933.x, gb933.k) FROM ga933, gb933
  WHERE ga933.k = gb933.k ORDER BY 1 DESC LIMIT 3;
SELECT left(string_agg(ga933.x::text, ',' ORDER BY ga933.x + gb933.y, ga933.x), 60)
  FROM ga933, gb933 WHERE ga933.k = gb933.k;
SET enable_hashjoin = off;
SET enable_mergejoin = off;
EXPLAIN SELECT * FROM ga933, gb933, gc933 WHERE ga933.x + gb933.y = gc933.z AND ga933.k = gb933.k;
SELECT count(*), sum(gc933.w) FROM ga933, gb933, gc933
  WHERE ga933.x + gb933.y = gc933.z AND ga933.k = gb933.k;
-- A nested loop probing an index once per outer row reads only the rows the
-- index finds for each.
SELECT count(*), sum(gc933.w) FROM ga933, gc933 WHERE ga933.x = gc933.z;
RESET enable_hashjoin;
RESET enable_mergejoin;
DROP TABLE ga933, gb933, gc933;
