-- PostgreSQL's join search over left, right and full joins: right joins
-- become left joins with their sides swapped and joins a condition above
-- them rejects NULLs for become inner joins (reduce_outer_joins), a left
-- join whose rows a WHERE IS NULL test keeps unmatched becomes an anti join,
-- each outer join limits the join order (make_outerjoininfo, join_is_legal),
-- left joins commute when their conditions allow (identity 3), and each
-- join's conditions split into the ones it matches by (Join Filter) and
-- the filters pushed down to it (Filter). Hash, merge and nested-loop joins
-- run as PostgreSQL's executor runs them, returning unmatched rows with
-- NULLs: a hash join's unmatched inner rows in bucket order.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/prep/prepjointree.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/plan/initsplan.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/joinrels.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/nodeHashjoin.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/nodeMergejoin.c
CREATE TABLE oa846 (id int, x int, t text);
CREATE TABLE ob846 (id int, a_id int, y int);
CREATE TABLE oc846 (k int, z int);
INSERT INTO oa846 SELECT g, g % 10, 'a' || g FROM generate_series(1, 60) g;
INSERT INTO ob846 SELECT g, CASE WHEN g % 9 = 0 THEN NULL ELSE g % 70 END, g FROM generate_series(1, 80) g;
INSERT INTO oc846 SELECT g % 15, g FROM generate_series(1, 40) g;
ANALYZE oa846; ANALYZE ob846; ANALYZE oc846;
EXPLAIN SELECT oa846.id, ob846.id, ob846.y FROM oa846 LEFT JOIN ob846 ON oa846.id = ob846.a_id AND ob846.y < 50;
SELECT oa846.id, ob846.id, ob846.y FROM oa846 LEFT JOIN ob846 ON oa846.id = ob846.a_id AND ob846.y < 50;
EXPLAIN SELECT oa846.id, ob846.id FROM oa846 RIGHT JOIN ob846 ON oa846.id = ob846.a_id AND oa846.x > 3;
SELECT oa846.id, ob846.id FROM oa846 RIGHT JOIN ob846 ON oa846.id = ob846.a_id AND oa846.x > 3;
EXPLAIN SELECT oa846.id, ob846.id FROM oa846 FULL JOIN ob846 ON oa846.id = ob846.a_id;
SELECT oa846.id, ob846.id FROM oa846 FULL JOIN ob846 ON oa846.id = ob846.a_id;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT oa846.id, ob846.id FROM oa846 FULL JOIN ob846 ON oa846.id = ob846.a_id;
-- A WHERE condition rejecting the nullable side's NULLs makes the join
-- inner; IS NULL of a column the join clause compares makes it an anti
-- join; IS NULL of another column is a filter above the join.
EXPLAIN SELECT oa846.id, ob846.id FROM oa846 LEFT JOIN ob846 ON oa846.id = ob846.a_id WHERE ob846.y > 10;
EXPLAIN SELECT oa846.id, ob846.id FROM oa846 LEFT JOIN ob846 ON oa846.id = ob846.a_id WHERE ob846.a_id IS NULL;
SELECT oa846.id, ob846.id FROM oa846 LEFT JOIN ob846 ON oa846.id = ob846.a_id WHERE ob846.a_id IS NULL;
EXPLAIN SELECT oa846.id, ob846.id FROM oa846 LEFT JOIN ob846 ON oa846.id = ob846.a_id WHERE ob846.y IS NULL;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT oa846.id, ob846.id FROM oa846 LEFT JOIN ob846 ON oa846.id = ob846.a_id WHERE ob846.y IS NULL;
-- Left joins that commute (identity 3), and a filter on a relation two
-- outer joins null.
EXPLAIN SELECT oa846.id, ob846.id, oc846.z FROM oa846 LEFT JOIN ob846 ON oa846.id = ob846.a_id
  LEFT JOIN oc846 ON oc846.k = ob846.y;
SELECT oa846.id, ob846.id, oc846.z FROM oa846 LEFT JOIN ob846 ON oa846.id = ob846.a_id
  LEFT JOIN oc846 ON oc846.k = ob846.y;
EXPLAIN SELECT oa846.id, ob846.id, oc846.z FROM oa846 LEFT JOIN ob846 ON oa846.id = ob846.a_id
  LEFT JOIN oc846 ON oc846.k = ob846.y WHERE oc846.z IS NULL;
EXPLAIN SELECT oa846.id, ob846.id, oc846.z FROM oc846 RIGHT JOIN ob846 ON oc846.k = ob846.y
  FULL JOIN oa846 ON oa846.id = ob846.a_id;
SELECT oa846.id, ob846.id, oc846.z FROM oc846 RIGHT JOIN ob846 ON oc846.k = ob846.y
  FULL JOIN oa846 ON oa846.id = ob846.a_id;
-- Merge and nested-loop forms.
SET enable_hashjoin = off;
EXPLAIN SELECT oa846.id, ob846.id, ob846.y FROM oa846 LEFT JOIN ob846 ON oa846.id = ob846.a_id AND ob846.y < 50;
SELECT oa846.id, ob846.id, ob846.y FROM oa846 LEFT JOIN ob846 ON oa846.id = ob846.a_id AND ob846.y < 50;
EXPLAIN SELECT oa846.id, ob846.id FROM oa846 FULL JOIN ob846 ON oa846.id = ob846.a_id;
SELECT oa846.id, ob846.id FROM oa846 FULL JOIN ob846 ON oa846.id = ob846.a_id;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT oa846.id, ob846.id FROM oa846 FULL JOIN ob846 ON oa846.id = ob846.a_id;
EXPLAIN SELECT oa846.id, ob846.id FROM oa846 LEFT JOIN ob846 ON oa846.id = ob846.a_id WHERE ob846.a_id IS NULL;
SELECT oa846.id, ob846.id FROM oa846 LEFT JOIN ob846 ON oa846.id = ob846.a_id WHERE ob846.a_id IS NULL;
SET enable_mergejoin = off;
EXPLAIN SELECT oa846.id, ob846.id, ob846.y FROM oa846 LEFT JOIN ob846 ON oa846.id = ob846.a_id AND ob846.y < 50;
SELECT oa846.id, ob846.id, ob846.y FROM oa846 LEFT JOIN ob846 ON oa846.id = ob846.a_id AND ob846.y < 50;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT oa846.id, ob846.id FROM oa846 LEFT JOIN ob846 ON oa846.id = ob846.a_id WHERE ob846.a_id IS NULL;
EXPLAIN SELECT oa846.id, ob846.id FROM oa846 FULL JOIN ob846 ON oa846.id = ob846.a_id;
RESET enable_hashjoin; RESET enable_mergejoin;
-- A parameterized index scan of the nullable side, and Memoize over it.
CREATE INDEX ob846_a_id ON ob846 (a_id);
EXPLAIN SELECT oa846.id, ob846.id, ob846.y FROM oa846 LEFT JOIN ob846 ON oa846.id = ob846.a_id AND ob846.y < 50;
SELECT oa846.id, ob846.id, ob846.y FROM oa846 LEFT JOIN ob846 ON oa846.id = ob846.a_id AND ob846.y < 50;
EXPLAIN SELECT oc846.k, ob846.id FROM oc846 LEFT JOIN ob846 ON oc846.k = ob846.a_id;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT oc846.k, ob846.id FROM oc846 LEFT JOIN ob846 ON oc846.k = ob846.a_id;
-- A full join needs mergeable or hashable conditions.
SELECT oa846.id, ob846.id FROM oa846 FULL JOIN ob846 ON oa846.id < ob846.a_id;
