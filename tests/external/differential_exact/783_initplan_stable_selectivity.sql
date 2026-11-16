-- A comparison of a column with an InitPlan's output is estimated as one
-- with a constant of unknown value (`var_eq_non_const`, `DEFAULT_INEQ_SEL`),
-- and the InitPlan's parameter is an index condition; a stable expression is
-- estimated from its value at planning (`estimate_expression_value`) and its
-- evaluation is charged once to the index scan's startup.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/adt/selfuncs.c
CREATE TABLE t783 (a int, b int, c timestamptz, d date);
INSERT INTO t783 SELECT g, g % 10, '2024-01-01'::timestamptz + g * interval '1 hour', '2024-01-01'::date + (g % 100) FROM generate_series(1, 2000) g;
CREATE INDEX t783_a ON t783 (a);
CREATE INDEX t783_b ON t783 (b);
CREATE INDEX t783_c ON t783 (c);
ANALYZE t783;
EXPLAIN SELECT * FROM t783 WHERE a = (SELECT 5);
EXPLAIN SELECT * FROM t783 WHERE a < (SELECT 5);
EXPLAIN SELECT * FROM t783 WHERE b = (SELECT 5);
EXPLAIN SELECT * FROM t783 WHERE b > (SELECT 5);
EXPLAIN SELECT * FROM t783 WHERE b <> (SELECT 3);
EXPLAIN SELECT a, b FROM t783 WHERE a >= (SELECT 1990) AND b = 2;
EXPLAIN SELECT * FROM t783 WHERE c > now();
EXPLAIN SELECT * FROM t783 WHERE c < now();
EXPLAIN SELECT * FROM t783 WHERE c > now() - interval '1 day';
EXPLAIN SELECT * FROM t783 WHERE d = current_date;
SELECT a, b FROM t783 WHERE a = (SELECT 5);
SELECT count(*) FROM t783 WHERE a < (SELECT 50);
SELECT count(*) FROM t783 WHERE b > (SELECT 5);
SELECT count(*) FROM t783 WHERE a = (SELECT NULL::int);
SELECT count(*) FROM t783 WHERE c > now() - interval '1 day';
