-- A row constructor compared with a subquery's single row field by field,
-- with row comparison NULL semantics and PostgreSQL's row and column errors.
CREATE TABLE t217 (a int, b int); CREATE TABLE u217 (x int, y int);
INSERT INTO t217 VALUES (1, 2), (3, 4), (5, NULL); INSERT INTO u217 VALUES (1, 2), (3, 5);
SELECT a, b, (a, b) = (SELECT x, y FROM u217 WHERE x = t217.a) AS eq, (a, b) <> (SELECT x, y FROM u217 WHERE x = t217.a) AS ne, (a, b) < (SELECT x, y FROM u217 WHERE x = t217.a) AS lt FROM t217 ORDER BY a;
SELECT * FROM t217 WHERE (a, b) = (SELECT 1, 2);
SELECT * FROM t217 WHERE (a, b) IN (SELECT x, y FROM u217);
SELECT * FROM t217 WHERE (a, b) NOT IN (SELECT x, y FROM u217);
SELECT * FROM t217 WHERE (a, b) = ANY (SELECT x, y FROM u217);
SELECT * FROM t217 WHERE (a, b) < ALL (SELECT x, y FROM u217);
SELECT (1, 2) = (SELECT x, y FROM u217);
SELECT (1, 2) = (SELECT x FROM u217 WHERE x = 1);
SELECT (1, 2, 3) = (SELECT x, y FROM u217 WHERE x = 1);
SELECT (1, 2) = (SELECT x, y FROM u217 WHERE false);
SELECT (1, NULL) = (SELECT 1, 2), (1, NULL) <> (SELECT 2, 2), (1, NULL) >= (SELECT 1, 2);
SELECT ROW(1, 2) = (SELECT x, y FROM u217 WHERE x = 1);
SELECT (1, 2) IS DISTINCT FROM (SELECT 1, 2);
DROP TABLE t217, u217;
