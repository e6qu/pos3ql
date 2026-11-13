-- A missing column in a set operation's later branch is offered the
-- earlier branches' columns as `*SELECT* n`, after the branch's own FROM
-- items, as PostgreSQL's parent range table holds them.
CREATE TABLE t362 (id int, val int);
SELECT 1 AS x UNION SELECT x2;
SELECT 1 AS x, 2 AS yy UNION SELECT 3, 4 UNION SELECT x1, 5;
SELECT id FROM t362 UNION SELECT vall FROM t362;
SELECT id FROM t362 UNION SELECT idd FROM t362;
SELECT 1 AS idx UNION SELECT idd FROM t362;
SELECT * FROM (SELECT 1 AS x UNION SELECT x2) s;
SELECT 1 AS x EXCEPT SELECT xx;
SELECT 1 AS x INTERSECT ALL SELECT x3;
VALUES (1) UNION SELECT column2;
SELECT id FROM t362 UNION SELECT 1 WHERE idd > 0;
DROP TABLE t362;
