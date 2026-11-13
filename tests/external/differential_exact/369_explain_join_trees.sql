-- EXPLAIN of three or more relations prints the nested pairwise join tree
-- pos3ql runs, with each scan's filter and each join's filter at the join
-- that adds the last relation it reads; constants fixed through chains of
-- equal columns filter every member's scan. Aliased relations print both
-- names.
CREATE TABLE a369 (x int, y int);
CREATE TABLE b369 (x int, z int);
CREATE TABLE c369 (x int, w int);
EXPLAIN (COSTS OFF) SELECT * FROM a369, b369, c369 WHERE a369.x = 1 AND b369.x = 1 AND c369.x = 1;
EXPLAIN (COSTS OFF) SELECT * FROM a369 JOIN b369 ON a369.y < b369.z
  JOIN c369 ON b369.z <> c369.w WHERE c369.w > 3;
EXPLAIN (COSTS OFF) SELECT * FROM a369 JOIN b369 ON a369.x = b369.x
  JOIN c369 ON b369.x = c369.x WHERE a369.x = 7;
EXPLAIN (COSTS OFF) DELETE FROM a369 USING b369, c369 WHERE a369.y < b369.z AND b369.z < c369.w;
EXPLAIN (COSTS OFF) SELECT * FROM a369 q WHERE q.x = 1;
EXPLAIN (COSTS OFF) UPDATE a369 q SET y = 1 WHERE q.x = 2;
EXPLAIN (COSTS OFF) UPDATE a369 q SET y = 1 FROM b369 r WHERE q.y < r.z;
EXPLAIN (COSTS OFF) DELETE FROM a369 q USING b369 WHERE q.y < b369.z AND b369.x = 4;
