-- Uncorrelated subqueries in a statement read row by row: a SQL-language
-- function call in the output runs the SELECT in place, and its scalar,
-- EXISTS, ARRAY and row subqueries run once before the first row. Their
-- values, NULL and type results, and errors match PostgreSQL; correlated,
-- IN and ANY subqueries keep the statement in the general executor.
CREATE TABLE t (id int, v text);
INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, NULL);
CREATE TABLE u (k int, w varchar(5));
INSERT INTO u VALUES (2, 'two'), (3, 'three');
CREATE TABLE z (x int);
INSERT INTO z VALUES (0);
CREATE FUNCTION twice(int) RETURNS int LANGUAGE sql AS 'SELECT $1 * 2';
SELECT id, twice(id), (SELECT max(k) FROM u) AS top FROM t;
SELECT id, twice(id) FROM t WHERE id > (SELECT min(k) FROM u);
SELECT id, twice(id), (SELECT w FROM u WHERE k = 3) FROM t WHERE EXISTS (SELECT 1 FROM u WHERE k = 2);
SELECT id, twice(id) FROM t WHERE NOT EXISTS (SELECT 1 FROM u WHERE k = 9);
SELECT id, twice(id), ARRAY(SELECT k FROM u ORDER BY k DESC) FROM t;
SELECT id, twice(id), (SELECT k FROM u WHERE k > 9) IS NULL AS missing FROM t;
SELECT id, twice(id) FROM t WHERE v = (SELECT 'c'::text) OR id = (SELECT 1);
SELECT id, twice(id), (SELECT w FROM u ORDER BY k LIMIT 1) || v FROM t;
SELECT id, twice(id), (SELECT k FROM u) FROM t;
SELECT id, twice(id), (SELECT 1 / x FROM z) FROM t;
SELECT t.id, twice(t.id), u.w FROM t JOIN u ON u.k = t.id WHERE t.id < (SELECT max(k) FROM u);
SELECT id, twice(id), (SELECT max(k) FROM u WHERE k <= t.id) FROM t;
SELECT id, twice(id) FROM t WHERE id IN (SELECT k FROM u);
SELECT id, twice(id) FROM t WHERE id > ANY (SELECT k FROM u);
SELECT id, twice(id), (SELECT count(*) FROM t) FROM t LIMIT 2 OFFSET 1;
SELECT id, twice(id), (SELECT max(k) FROM u) FROM t WHERE id > 10;
