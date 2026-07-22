-- Row-constructor membership: `(a, b) IN (...)`.
--
-- Two things this pins down. A row comparison is three-valued, so a NULL
-- *inside* a row makes the membership unknown rather than a plain false — the
-- total order ORDER BY uses would treat that NULL as just another value. And
-- the right-hand side may be a subquery, whose columns form the row that is
-- compared, including when that subquery is a set operation.

CREATE TABLE rowmem (a int, b text);
INSERT INTO rowmem VALUES (1, 'x'), (2, 'y'), (3, NULL);
CREATE TABLE rowmem2 (a int, c int);
INSERT INTO rowmem2 VALUES (1, 10), (1, 20), (4, 40);

-- three-valued membership against a literal list
SELECT (1,2) IN ((1,2));
SELECT (1,2) IN ((3,4));
SELECT (1,NULL) IN ((1,NULL));
SELECT (1,2) IN ((1,NULL));
SELECT (1,2) NOT IN ((1,NULL));
SELECT (1,2) IN ((1,2),(1,NULL));
SELECT (1,2) IN ((3,4),(5,NULL));
SELECT (1,2) NOT IN ((3,4),(5,6));
SELECT (NULL,NULL) IN ((1,2));

-- the row operators themselves, for contrast
SELECT (1,2) = (1,NULL);
SELECT (1,2) = (3,NULL);
SELECT (1,2) <> (3,NULL);
SELECT (1,2) < (1,NULL);
SELECT (1,2) < (3,NULL);
SELECT (1,2) IS NULL;
SELECT (NULL,NULL) IS NULL;
SELECT (1,NULL) IS NULL;

-- against a subquery
SELECT (1,2) IN (SELECT 1,2);
SELECT ROW(1,2) IN (SELECT 1,2);
SELECT (1,2) IN (SELECT 3,4);
SELECT (1,2) IN (SELECT 1,2 WHERE false);
SELECT (1,2) NOT IN (SELECT 1,2 WHERE false);
SELECT a FROM rowmem WHERE (a,a) IN (SELECT a, a FROM rowmem2) ORDER BY a;
SELECT a FROM rowmem WHERE (a,b) IN (SELECT a,b FROM rowmem) ORDER BY a;
SELECT a, c FROM rowmem2 WHERE (a,c) NOT IN (SELECT a,c FROM rowmem2 WHERE c = 10) ORDER BY 1, 2;
SELECT a FROM rowmem WHERE (a,a) IN (SELECT a,a FROM rowmem2 ORDER BY a LIMIT 1);

-- correlated
SELECT a FROM rowmem WHERE (a,a) IN (SELECT r2.a, r2.a FROM rowmem2 r2 WHERE r2.a = rowmem.a) ORDER BY a;

-- against a set operation, whose branches combine column-wise before the rows
-- are compared
SELECT (1,2) IN (SELECT 1,2 UNION SELECT 9,9);
SELECT (1,2) IN (SELECT 1,2 UNION ALL SELECT 9,9);
SELECT (5,5) IN (SELECT 1,2 UNION SELECT 9,9);
SELECT (1,2) IN (SELECT 1,2 UNION SELECT 1,2);
SELECT (1,2) NOT IN (SELECT 1,2 UNION SELECT 9,9);
SELECT (1,2) IN (SELECT 9,9 WHERE false UNION SELECT 8,8 WHERE false);
SELECT ('x',1) IN (SELECT 'x',1 UNION SELECT 'y',2);
SELECT a, c FROM rowmem2 WHERE (a,c) IN (SELECT a,c FROM rowmem2 UNION SELECT 9,9) ORDER BY 1, 2;
SELECT a, c FROM rowmem2 WHERE (a,c) IN (SELECT a,c FROM rowmem2 INTERSECT SELECT 1,10) ORDER BY 1, 2;
SELECT a, c FROM rowmem2 WHERE (a,c) IN (SELECT a,c FROM rowmem2 EXCEPT SELECT 1,10) ORDER BY 1, 2;
SELECT a, c FROM rowmem2 WHERE (a,c) IN (SELECT a,c FROM rowmem2 UNION SELECT 9,9 ORDER BY 1 LIMIT 2) ORDER BY 1, 2;

-- arity is checked against the row, in PostgreSQL's words
SELECT a FROM rowmem WHERE (a,a) IN (SELECT a FROM rowmem2);
SELECT a FROM rowmem WHERE a IN (SELECT a, c FROM rowmem2);
SELECT a FROM rowmem WHERE (a,b) IN (SELECT a, c FROM rowmem2);

-- scalar membership is unchanged by any of it
SELECT 1 IN (1,NULL);
SELECT 2 IN (1,NULL);
SELECT 2 NOT IN (1,NULL);
SELECT a FROM rowmem WHERE a IN (SELECT a FROM rowmem2) ORDER BY a;
SELECT a FROM rowmem WHERE a NOT IN (SELECT a FROM rowmem2) ORDER BY a;
SELECT a FROM rowmem WHERE a IN (SELECT a FROM rowmem2 UNION SELECT 3) ORDER BY a;

DROP TABLE rowmem;
DROP TABLE rowmem2;
