-- PostgreSQL 18's EXPLAIN plans the statement eval_const_expressions
-- produced: each immutable operator and function over constants is
-- printed as its value, a comparison with a NULL constant folds to NULL,
-- a condition folded to false or NULL leaves a Result with a false
-- One-Time Filter (also under UPDATE and DELETE), a conjunct folded to
-- true is dropped, a list of constants becomes one array constant, and a
-- condition that reads no column gates the scan from a Result. Plain
-- EXPLAIN raises the folding's warnings; EXPLAIN ANALYZE raises them once.
CREATE TABLE t530 (a int, b int);
INSERT INTO t530 SELECT g, g FROM generate_series(1, 5) g;
EXPLAIN (COSTS OFF) SELECT * FROM t530 WHERE a > 1 + 1;
EXPLAIN (COSTS OFF) SELECT * FROM t530 WHERE b = length('abc') AND a <> abs(-5) + 0;
EXPLAIN (COSTS OFF) UPDATE t530 SET b = 1 WHERE 1 = 2;
EXPLAIN (COSTS OFF) DELETE FROM t530 WHERE 1 = 2;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) UPDATE t530 SET b = 1 WHERE 1 = 2;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) DELETE FROM t530 WHERE 1 = 2 RETURNING a;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) SELECT * FROM t530 WHERE 1 = 2;
EXPLAIN (COSTS OFF)
  SELECT a + (2 * 3) AS x, 'a' || 'b', upper('x') FROM t530 WHERE a < 10 - 1 ORDER BY a + (2 * 3);
SELECT a + (2 * 3), 'a' || 'b', upper('x'), 1 + 1 FROM t530 WHERE a < 10 - 1 ORDER BY 1 LIMIT 2;
EXPLAIN (COSTS OFF) SELECT * FROM t530 WHERE a = -5;
EXPLAIN (COSTS OFF) SELECT * FROM t530 WHERE a = NULL::int + 1;
EXPLAIN (COSTS OFF) SELECT * FROM t530 WHERE a::numeric > 1.5 * 2;
EXPLAIN (COSTS OFF) SELECT * FROM t530 WHERE a > 2 OR 1 = 1;
EXPLAIN (COSTS OFF) SELECT * FROM t530 WHERE a > 2 AND (1 = 1);
EXPLAIN (COSTS OFF) SELECT * FROM t530 WHERE (a, b) = (1, 1 + 1);
EXPLAIN (COSTS OFF) SELECT * FROM t530 LIMIT 1 + 1;
EXPLAIN (COSTS OFF) SELECT * FROM t530 WHERE a IN (1, 1 + 1, 3);
EXPLAIN (COSTS OFF) SELECT * FROM t530 WHERE a NOT IN (4, 2 * 3);
EXPLAIN (COSTS OFF) SELECT * FROM t530 WHERE a = ANY (ARRAY[1, 2 + 0]);
EXPLAIN (COSTS OFF) SELECT * FROM t530 WHERE now() > '2020-01-01'::date + 1;
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
  SELECT * FROM t530 WHERE now() > '2020-01-01'::date + 1 AND a > 3;
EXPLAIN (COSTS OFF) SELECT * FROM t530 WHERE a = CASE WHEN 1 > 0 THEN 2 ELSE 3 END;
EXPLAIN (COSTS OFF) SELECT * FROM t530 WHERE a::text = 'x' || 'y';
EXPLAIN (COSTS OFF) INSERT INTO t530 VALUES (1 + 1, abs(-3));
EXPLAIN (COSTS OFF) UPDATE t530 SET b = 2 * 3 WHERE a = 1 + 1;
EXPLAIN (COSTS OFF) DELETE FROM t530 WHERE a < 2 + 2;
EXPLAIN (COSTS OFF) SELECT timestamptypmodin('{7}'), a FROM t530 WHERE b > timestamptypmodin('{8}');
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
  SELECT timestamptypmodin('{7}'), a FROM t530 WHERE b > timestamptypmodin('{8}');
EXPLAIN (COSTS OFF) SELECT a FROM t530 WHERE a = 1 / 0;
SELECT * FROM t530 ORDER BY a;
