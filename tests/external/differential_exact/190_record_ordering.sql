-- ORDER BY, DISTINCT, GROUP BY, set operations, window ordering and
-- aggregate ordering over record values use record_cmp: fields compare by
-- their type (numeric 1.0 = 1.00, 9 < 10), NULL fields sort last and equal
-- each other. DISTINCT, GROUP BY and UNION keep each group's first row.
CREATE TABLE rt186(a int, b numeric, c text);
INSERT INTO rt186 VALUES (10, 1.0, 'x'), (9, 1.00, 'y'), (2, NULL, 'z'),
  (10, 1.00, 'x'), (100, 2, 'w'), (NULL, 1, 'v');
SELECT ROW(a, b) AS r FROM rt186 ORDER BY 1;
SELECT ROW(a, b) AS r FROM rt186 ORDER BY 1 DESC;
SELECT DISTINCT ROW(a, b) AS r FROM rt186 ORDER BY 1;
SELECT ROW(a, b) AS r, count(*) FROM rt186 GROUP BY 1 ORDER BY 1;
SELECT ROW(a, b) FROM rt186 UNION SELECT ROW(a, b) FROM rt186 ORDER BY 1;
SELECT ROW(a, b) FROM rt186 INTERSECT SELECT ROW(a, b) FROM rt186 ORDER BY 1;
SELECT ROW(a, b) FROM rt186 EXCEPT SELECT ROW(10, 1::numeric) ORDER BY 1;
SELECT r FROM (SELECT ROW(c, a) AS r FROM rt186) s ORDER BY r;
SELECT array_agg(ROW(a, b) ORDER BY ROW(a, b)) FROM rt186;
SELECT DISTINCT ON (ROW(a, b)) a, b, c FROM rt186 ORDER BY ROW(a, b), c;
SELECT ROW(a, b), rank() OVER (ORDER BY ROW(a, b)) FROM rt186 ORDER BY 2, 1;
SELECT min(a) FROM rt186 GROUP BY ROW(a, b) ORDER BY 1;
SELECT rt186 FROM rt186 ORDER BY rt186;
SELECT x FROM (VALUES (ARRAY[ROW(10, 1.0)]), (ARRAY[ROW(9, 2.0)]), (ARRAY[ROW(10, 1.00)])) v(x) ORDER BY x;
SELECT DISTINCT x FROM (VALUES (ARRAY[ROW(10, 1.0)]), (ARRAY[ROW(9, 2.0)]), (ARRAY[ROW(10, 1.00)])) v(x) ORDER BY x;
SELECT x FROM (VALUES (ARRAY[ROW(1), NULL]), (ARRAY[ROW(1), ROW(0)])) v(x) ORDER BY x;
SELECT ARRAY[ROW(1), NULL];
SELECT DISTINCT x FROM (VALUES (1.0), (1.00)) v(x);
SELECT x, count(*) FROM (VALUES (1.0), (1.00)) v(x) GROUP BY x;
SELECT DISTINCT max(x) FROM (VALUES (1, 1.0), (2, 1.00)) v(g, x) GROUP BY g;
SELECT count(*) FROM (SELECT 1.0 UNION SELECT 1.00) s;
WITH RECURSIVE t(x) AS (SELECT 1.0 UNION SELECT 1.00 FROM t) SELECT * FROM t;
WITH RECURSIVE t(r) AS (SELECT ROW(1.0) UNION SELECT ROW(1.00) FROM t) SELECT * FROM t;
CREATE TYPE pt186 AS (x int, y numeric);
CREATE TABLE ct186(p pt186);
INSERT INTO ct186 VALUES ((10, 1.0)), ((9, 2)), ((100, 1)), (NULL), ((10, 1.00)), ((NULL, 1));
SELECT p FROM ct186 ORDER BY p;
SELECT DISTINCT p FROM ct186 ORDER BY p;
SELECT p, count(*) FROM ct186 GROUP BY p ORDER BY p;
SELECT array_agg(p ORDER BY p) FROM ct186;
SELECT r, count(*) FROM (VALUES (ROW(10, 1.0)), (ROW(9, 1)), (ROW(10, 1.00))) v(r) GROUP BY r ORDER BY r;
SELECT array_agg(r ORDER BY r) FROM (SELECT ROW(x) r FROM (VALUES (10), (9)) v(x)) s;
SELECT ROW(10) = ANY (ARRAY[ROW(9), ROW(10.0)]);
SELECT ROW(1.0) = ANY (ARRAY(SELECT ROW(1.00)));
SELECT json_object_agg('k', ROW(1, ARRAY[ROW(2)])), jsonb_object_agg('k', ROW(1, 2));
SELECT json_agg(ROW(1, ARRAY[ROW(2)])), jsonb_agg(ROW(1, 2));
SELECT ROW(ROW(10, 'a')) AS n UNION SELECT ROW(ROW(9, 'b')) ORDER BY 1;
SELECT r FROM (SELECT ROW(x) r FROM (VALUES (10), (9), (100)) v(x)) s ORDER BY r FETCH FIRST 1 ROWS WITH TIES;
SELECT ROW(1, 'a') = ROW(1, 'a'::text);
SELECT r FROM (VALUES (ROW(1)), (ROW('a'::text))) v(r) ORDER BY 1;
SELECT row_number() OVER (), r FROM (VALUES (ROW(1)), (ROW('a'::text))) v(r) ORDER BY r;
SELECT ARRAY(SELECT r FROM (VALUES (ROW(1)), (ROW('a'::text))) v(r) ORDER BY r);
SELECT x, ARRAY(SELECT y FROM (VALUES (ROW(2)), (ROW(1))) w(y) ORDER BY y) FROM (VALUES (1)) v(x);
