-- USING chains planned by the join search. Each join's merged column is
-- PostgreSQL's join alias variable (buildMergedJoinVar): the left column
-- for an inner or left join, the right one for a right join and their
-- COALESCE for a full join, so a chain's later joins compare the first
-- relation's column and every column joins one equivalence class. Chains
-- longer than the search's former 16-member class, 32-clause join and
-- 48-condition capacities plan as PostgreSQL plans them.
CREATE TABLE uc (id int, v int);
INSERT INTO uc SELECT g, g % 7 FROM generate_series(1, 200) g;
ANALYZE uc;
CREATE TABLE ub (id bigint, w int);
INSERT INTO ub SELECT g, g % 5 FROM generate_series(1, 50) g;
ANALYZE ub;

EXPLAIN (COSTS OFF) SELECT id FROM uc t0 JOIN uc t1 USING (id) JOIN uc t2 USING (id);
SELECT id, t2.v FROM uc t0 JOIN uc t1 USING (id) JOIN uc t2 USING (id) WHERE id < 4 ORDER BY id;
EXPLAIN SELECT id FROM uc t0 JOIN uc t1 USING (id) JOIN uc t2 USING (id) WHERE id < 50;
EXPLAIN (VERBOSE, COSTS OFF)
  SELECT id FROM uc t0 LEFT JOIN uc t1 USING (id) RIGHT JOIN uc t2 USING (id);
EXPLAIN (VERBOSE, COSTS OFF)
  SELECT id FROM uc t0 JOIN uc t1 USING (id) JOIN uc t2 USING (id);
SELECT count(*), sum(id) FROM uc t0 FULL JOIN uc t1 USING (id) FULL JOIN uc t2 USING (id);
-- A full join chain compares the lower joins' COALESCE, an expression of
-- several relations that hash and merge joins key by.
EXPLAIN (COSTS OFF) SELECT count(*), sum(id) FROM uc t0 FULL JOIN uc t1 USING (id)
  FULL JOIN uc t2 USING (id) FULL JOIN uc t3 USING (id);
EXPLAIN (COSTS OFF)
  SELECT id FROM uc t0 FULL JOIN uc t1 USING (id) JOIN uc t2 USING (id) LEFT JOIN uc t3 USING (id);
SELECT id, t3.v FROM uc t0 FULL JOIN uc t1 USING (id) JOIN uc t2 USING (id)
  LEFT JOIN uc t3 USING (id) WHERE id < 5 ORDER BY id;
SELECT count(*), sum(id) FROM uc t0 FULL JOIN (SELECT * FROM uc WHERE id > 150) t1 USING (id)
  FULL JOIN (SELECT * FROM uc WHERE id < 20) t2 USING (id);
-- A merged column of two types reads its sides coerced to the common type.
SELECT pg_typeof(id), id FROM uc LEFT JOIN ub USING (id) WHERE uc.id = 3;
SELECT pg_typeof(id), id FROM ub FULL JOIN uc USING (id) WHERE uc.id = 60;

-- 17 relations: one class of 17 members.
EXPLAIN (COSTS OFF) SELECT id FROM uc t0 JOIN uc t1 USING (id) JOIN uc t2 USING (id)
  JOIN uc t3 USING (id) JOIN uc t4 USING (id) JOIN uc t5 USING (id) JOIN uc t6 USING (id)
  JOIN uc t7 USING (id) JOIN uc t8 USING (id) JOIN uc t9 USING (id) JOIN uc t10 USING (id)
  JOIN uc t11 USING (id) JOIN uc t12 USING (id) JOIN uc t13 USING (id) JOIN uc t14 USING (id)
  JOIN uc t15 USING (id) JOIN uc t16 USING (id);
-- 34 relations on two columns: 66 conditions.
EXPLAIN (COSTS OFF) SELECT t0.id, t33.v FROM uc t0 JOIN uc t1 USING (id, v)
  JOIN uc t2 USING (id, v) JOIN uc t3 USING (id, v) JOIN uc t4 USING (id, v)
  JOIN uc t5 USING (id, v) JOIN uc t6 USING (id, v) JOIN uc t7 USING (id, v)
  JOIN uc t8 USING (id, v) JOIN uc t9 USING (id, v) JOIN uc t10 USING (id, v)
  JOIN uc t11 USING (id, v) JOIN uc t12 USING (id, v) JOIN uc t13 USING (id, v)
  JOIN uc t14 USING (id, v) JOIN uc t15 USING (id, v) JOIN uc t16 USING (id, v)
  JOIN uc t17 USING (id, v) JOIN uc t18 USING (id, v) JOIN uc t19 USING (id, v)
  JOIN uc t20 USING (id, v) JOIN uc t21 USING (id, v) JOIN uc t22 USING (id, v)
  JOIN uc t23 USING (id, v) JOIN uc t24 USING (id, v) JOIN uc t25 USING (id, v)
  JOIN uc t26 USING (id, v) JOIN uc t27 USING (id, v) JOIN uc t28 USING (id, v)
  JOIN uc t29 USING (id, v) JOIN uc t30 USING (id, v) JOIN uc t31 USING (id, v)
  JOIN uc t32 USING (id, v) JOIN uc t33 USING (id, v);
SELECT count(*), sum(id) FROM uc t0 JOIN uc t1 USING (id, v)
  JOIN uc t2 USING (id, v) JOIN uc t3 USING (id, v) JOIN uc t4 USING (id, v)
  JOIN uc t5 USING (id, v) JOIN uc t6 USING (id, v) JOIN uc t7 USING (id, v)
  JOIN uc t8 USING (id, v) JOIN uc t9 USING (id, v) JOIN uc t10 USING (id, v)
  JOIN uc t11 USING (id, v) JOIN uc t12 USING (id, v) JOIN uc t13 USING (id, v)
  JOIN uc t14 USING (id, v) JOIN uc t15 USING (id, v) JOIN uc t16 USING (id, v)
  JOIN uc t17 USING (id, v) JOIN uc t18 USING (id, v) JOIN uc t19 USING (id, v)
  JOIN uc t20 USING (id, v) JOIN uc t21 USING (id, v) JOIN uc t22 USING (id, v)
  JOIN uc t23 USING (id, v) JOIN uc t24 USING (id, v) JOIN uc t25 USING (id, v)
  JOIN uc t26 USING (id, v) JOIN uc t27 USING (id, v) JOIN uc t28 USING (id, v)
  JOIN uc t29 USING (id, v) JOIN uc t30 USING (id, v) JOIN uc t31 USING (id, v)
  JOIN uc t32 USING (id, v) JOIN uc t33 USING (id, v)
  WHERE id < 30;
DROP TABLE uc, ub;
