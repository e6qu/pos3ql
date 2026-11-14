-- PostgreSQL 18 runs a subquery it does not pull up as a subplan: once
-- before the first row (an InitPlan, referenced as `(InitPlan n).col1`) or
-- per row when it reads the enclosing query's columns (a SubPlan,
-- `(SubPlan n)`). Subplans are numbered as planned, the subqueries inside
-- one first; InitPlans hang under the query's top node, SubPlans after the
-- inputs of the node that evaluates them, and a condition reading only
-- InitPlans gates the scan from a Result. pos3ql does not count a
-- subplan's operators under ANALYZE yet, so this compares plain plans.
CREATE TABLE t474 (a int, b int);
INSERT INTO t474 SELECT g % 10, g FROM generate_series(1, 100) g;
CREATE TABLE u474 (a int, c text);
INSERT INTO u474 SELECT g, 'x' || g FROM generate_series(1, 20) g;
ANALYZE t474;
ANALYZE u474;
EXPLAIN (COSTS OFF) SELECT * FROM t474 WHERE b = (SELECT count(*) FROM u474);
EXPLAIN (COSTS OFF) SELECT (SELECT count(*) FROM u474), a FROM t474;
EXPLAIN (COSTS OFF) SELECT (SELECT count(*) FROM u474), a FROM t474 WHERE b > (SELECT count(*) FROM u474 u2);
EXPLAIN (COSTS OFF) SELECT a, (SELECT count(*) FROM u474) FROM t474 ORDER BY b LIMIT 2;
EXPLAIN (COSTS OFF) SELECT * FROM t474 WHERE b > (SELECT count(*) FROM u474) ORDER BY b;
EXPLAIN (COSTS OFF) SELECT a, (SELECT c FROM u474 WHERE u474.a = t474.b) FROM t474;
EXPLAIN (COSTS OFF) SELECT * FROM t474 WHERE b = (SELECT u474.a FROM u474 WHERE u474.a = t474.b) OR a = 3;
EXPLAIN (COSTS OFF) SELECT a FROM t474 WHERE b = (SELECT max(b) FROM t474 t2 WHERE t2.a = t474.a + (SELECT count(*) FROM u474));
EXPLAIN (COSTS OFF) SELECT ARRAY(SELECT a FROM u474 WHERE u474.a < t474.a) FROM t474;
EXPLAIN (COSTS OFF) SELECT a FROM t474 GROUP BY a HAVING count(*) > (SELECT count(*) FROM u474);
EXPLAIN (COSTS OFF) SELECT * FROM t474 WHERE b < (SELECT count(*) FROM u474 WHERE u474.a > t474.a);
EXPLAIN (COSTS OFF) SELECT * FROM t474 WHERE EXISTS (SELECT 1 FROM u474);
EXPLAIN (COSTS OFF) SELECT * FROM t474 WHERE a = 1 AND EXISTS (SELECT 1 FROM u474 WHERE a > 5);
EXPLAIN (COSTS OFF) SELECT * FROM t474 WHERE a = 1 AND EXISTS (SELECT 1 FROM u474 WHERE a > 5) ORDER BY b;
EXPLAIN (COSTS OFF) SELECT (SELECT a FROM u474 ORDER BY a LIMIT 1) FROM t474;
EXPLAIN (COSTS OFF) SELECT * FROM t474 JOIN u474 ON t474.b = u474.a WHERE t474.b > (SELECT count(*) FROM u474);
EXPLAIN (COSTS OFF, FORMAT JSON) SELECT * FROM t474 WHERE b = (SELECT count(*) FROM u474);
EXPLAIN (COSTS OFF, FORMAT YAML) SELECT a, (SELECT c FROM u474 WHERE u474.a = t474.b) FROM t474;
SELECT a, (SELECT c FROM u474 WHERE u474.a = t474.b) FROM t474 WHERE b < 4 ORDER BY b;
