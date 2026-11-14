-- PostgreSQL 18's EXPLAIN ANALYZE counts each subplan's nodes on their
-- own: an InitPlan's once, a SubPlan's once per row that runs it. A
-- query sorting rows whose select list calls a volatile function, or
-- under LIMIT an expensive one (a user routine or a correlated subplan),
-- evaluates that list after the sort, in a Result above it
-- (make_sort_input_target).
CREATE TABLE t531 (a int, b int);
INSERT INTO t531 SELECT g % 10, g FROM generate_series(1, 100) g;
CREATE TABLE u531 (k int, v int);
INSERT INTO u531 SELECT g, g * 2 FROM generate_series(1, 20) g;
CREATE FUNCTION f531() RETURNS bigint LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$;
CREATE FUNCTION s531() RETURNS bigint LANGUAGE plpgsql STABLE AS $$ BEGIN RETURN 1; END $$;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
  SELECT * FROM u531 WHERE v > (SELECT avg(b) FROM t531);
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
  SELECT k, (SELECT count(*) FROM t531 WHERE t531.a = u531.k) FROM u531 WHERE k < 4;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
  SELECT k, (SELECT max(v) FROM u531), (SELECT min(b) FROM t531) FROM u531 WHERE k < 3;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
  SELECT k FROM u531 WHERE k = (SELECT max(a) FROM t531 WHERE t531.b > u531.v);
EXPLAIN (COSTS OFF) SELECT a, f531() FROM t531 ORDER BY a;
EXPLAIN (COSTS OFF) SELECT a, f531() FROM t531 ORDER BY a LIMIT 1;
EXPLAIN (COSTS OFF) SELECT a, s531() FROM t531 ORDER BY a;
EXPLAIN (COSTS OFF) SELECT a, s531() FROM t531 ORDER BY a LIMIT 1;
EXPLAIN (COSTS OFF) SELECT a, random() > 2 FROM t531 ORDER BY b;
EXPLAIN (COSTS OFF) SELECT a, f531() FROM t531 ORDER BY 2;
EXPLAIN (COSTS OFF)
  SELECT a, (SELECT max(b) FROM t531 u WHERE u.a = t531.a) FROM t531 ORDER BY b LIMIT 1;
EXPLAIN (COSTS OFF)
  SELECT a, (SELECT max(b) FROM t531 u WHERE u.a = t531.a) FROM t531 ORDER BY b;
