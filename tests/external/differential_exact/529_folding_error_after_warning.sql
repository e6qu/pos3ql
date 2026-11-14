-- PostgreSQL 18's eval_const_expressions folds a statement's constants in
-- its planner's order; a constant that fails to fold is the statement's
-- error, sent after the warnings the constants folded before it raised.
-- A built-in call in an assignment has its catalog result type.
CREATE TABLE t529 (a int);
INSERT INTO t529 VALUES (1);
SELECT timestamptypmodin('{7}'), 1/0;
SELECT timestamptypmodin('{7}'), 1/0 FROM t529;
SELECT timestamptypmodin('{8}') + a, a % 0 FROM t529;
SELECT a FROM t529 WHERE timestamptypmodin('{7}') = 6 AND 1/0 = 1;
SELECT timestamptypmodin('{9}'), length((1/0)::text);
SELECT 1/0, timestamptypmodin('{7}');
SELECT a FROM t529 WHERE a = timestamptypmodin('{7}') ORDER BY 1/0;
UPDATE t529 SET a = timestamptypmodin('{7}') + 1/0;
INSERT INTO t529 VALUES (timestamptypmodin('{7}') + 0 * (1/0));
DELETE FROM t529 WHERE a = timestamptypmodin('{7}') RETURNING 1/0;
UPDATE t529 SET a = timestamptypmodin('{7}') + 1;
UPDATE t529 SET a = timestamptypmodin('{7}');
INSERT INTO t529 VALUES (int4in('5') + 1);
SELECT * FROM t529 ORDER BY a;
