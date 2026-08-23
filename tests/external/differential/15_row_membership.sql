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
SELECT ROW(1,10) IN (SELECT * FROM rowmem2 WHERE c = 10);
SELECT ROW(1,10) = ANY (SELECT r.* FROM rowmem2 AS r WHERE c = 10);
SELECT ROW(1,10) <= ALL (SELECT (ROW(a,c)).* FROM rowmem2 WHERE a = 1);
SELECT r.a, r.c FROM rowmem2 AS r
 WHERE r.* = ANY (SELECT x FROM rowmem2 AS x WHERE x.a = 1)
 ORDER BY 1, 2;
SELECT (1,2) IN (SELECT 3,4);
SELECT (1,2) IN (SELECT 1,2 WHERE false);
SELECT (1,2) NOT IN (SELECT 1,2 WHERE false);
SELECT a FROM rowmem WHERE (a,a) IN (SELECT a, a FROM rowmem2) ORDER BY a;
SELECT a FROM rowmem WHERE (a,b) IN (SELECT a,b FROM rowmem) ORDER BY a;
SELECT a, c FROM rowmem2 WHERE (a,c) NOT IN (SELECT a,c FROM rowmem2 WHERE c = 10) ORDER BY 1, 2;
SELECT a FROM rowmem WHERE (a,a) IN (SELECT a,a FROM rowmem2 ORDER BY a LIMIT 1);

-- quantified subqueries keep their row shape; they are not ARRAY(subquery)
SELECT ROW(1,2) = ANY (SELECT 1,2);
SELECT ROW(1,2) <> ANY (SELECT 1,2 UNION ALL SELECT 3,4);
SELECT ROW(1,2) = ALL (SELECT 1,2 UNION ALL SELECT 1,2);
SELECT ROW(1,2) < ANY (SELECT 1,1 UNION ALL SELECT 1,3);
SELECT ROW(1,2) < ALL (SELECT 1,3 UNION ALL SELECT 2,0);
SELECT ROW(1,2) > ANY (SELECT 9,9 WHERE false);
SELECT ROW(NULL,2) < ALL (SELECT 1,3 WHERE false);
SELECT ROW(1,NULL) = ANY (SELECT 1,NULL);
SELECT ROW(1,2) < ANY (SELECT 1,NULL);
SELECT ROW(1,2) < ANY (SELECT 1,NULL UNION ALL SELECT 2,0);
SELECT ROW(1,2) < ALL (SELECT 1,NULL UNION ALL SELECT 2,0);
SELECT 'a' COLLATE "C" = ANY (SELECT 'a' COLLATE "POSIX");
SELECT ROW('a' COLLATE "C",1) = ANY (SELECT 'a' COLLATE "POSIX",1);

-- A bounded two-pass query must not invoke a volatile table routine twice.
CREATE TABLE rowmem_effects (a int, b int);
CREATE FUNCTION rowmem_write(v int) RETURNS TABLE(a int, b int) LANGUAGE SQL AS
$$ INSERT INTO rowmem_effects VALUES (v, v + 1) RETURNING a, b $$;
SELECT ROW(7,8) = ANY (SELECT * FROM rowmem_write(7));
SELECT 9 = ANY (SELECT a FROM rowmem_write(9));
SELECT 11 = ANY (SELECT max(a) FROM rowmem_write(11));
SELECT * FROM rowmem_effects ORDER BY a;
DROP FUNCTION rowmem_write(int);
DROP TABLE rowmem_effects;

-- correlated
SELECT a FROM rowmem WHERE (a,a) IN (SELECT r2.a, r2.a FROM rowmem2 r2 WHERE r2.a = rowmem.a) ORDER BY a;
SELECT a FROM rowmem
 WHERE ROW(a,a) <= ALL (
       SELECT r2.a, r2.c FROM rowmem2 AS r2 WHERE r2.a = rowmem.a
 )
 ORDER BY a;

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
SELECT a FROM rowmem WHERE a < ANY (SELECT c FROM rowmem2) ORDER BY a;
SELECT a FROM rowmem WHERE a <= ALL (SELECT c FROM rowmem2) ORDER BY a;

-- arity errors apply to every quantified operator
SELECT ROW(1,2) < ANY (SELECT 1);
SELECT 1 < ALL (SELECT 1,2);

DROP TABLE rowmem;
DROP TABLE rowmem2;
