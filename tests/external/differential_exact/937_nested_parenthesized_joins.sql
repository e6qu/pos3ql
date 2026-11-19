-- A parenthesized join nested in another is a node of the join tree the
-- join search reads: its members join in the same search, and its query
-- never runs on its own, so its rows take no memory before the plan runs.
CREATE TABLE na937 (id int, x int);
CREATE TABLE nb937 (id int, y int);
CREATE TABLE nc937 (id int, z int);
CREATE TABLE nd937 (id int, w int);
INSERT INTO na937 SELECT g, g % 7 FROM generate_series(1, 400) g;
INSERT INTO nb937 SELECT g, g % 5 FROM generate_series(1, 2000) g;
INSERT INTO nc937 SELECT g, g % 3 FROM generate_series(1, 2000) g;
INSERT INTO nd937 SELECT g, g % 11 FROM generate_series(1, 2000) g;
ANALYZE na937, nb937, nc937, nd937;
EXPLAIN (COSTS OFF)
SELECT na937.id, nb937.y, nc937.z, nd937.w
  FROM na937 LEFT JOIN (nb937 JOIN (nc937 JOIN nd937 ON nc937.id = nd937.id)
    ON nb937.id = nc937.id) ON na937.id = nb937.id
  WHERE na937.x = 3;
SELECT na937.id, nb937.y, nc937.z, nd937.w
  FROM na937 LEFT JOIN (nb937 JOIN (nc937 JOIN nd937 ON nc937.id = nd937.id)
    ON nb937.id = nc937.id) ON na937.id = nb937.id
  WHERE na937.x = 3 ORDER BY na937.id LIMIT 5;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT na937.id, nd937.w
  FROM na937 JOIN (nb937 JOIN (nc937 JOIN nd937 ON nc937.id = nd937.id)
    ON nb937.id = nc937.id) ON na937.id = nb937.id
  WHERE na937.x = 3;
DROP TABLE na937, nb937, nc937, nd937;
