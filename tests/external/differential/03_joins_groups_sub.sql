-- Joins, GROUP BY/HAVING, aggregates, subqueries.
CREATE TABLE dept3 (id int, name text);
CREATE TABLE emp3 (id int, did int, name text, pay int);
INSERT INTO dept3 VALUES (1,'eng'),(2,'ops'),(3,'empty');
INSERT INTO emp3 VALUES (1,1,'ada',120),(2,1,'bob',100),(3,2,'cyd',90),(4,NULL,'dee',80);
SELECT e.name, d.name FROM emp3 e JOIN dept3 d ON e.did = d.id ORDER BY e.id;
SELECT e.name, d.name FROM emp3 e LEFT JOIN dept3 d ON e.did = d.id ORDER BY e.id;
SELECT d.name, e.name FROM dept3 d LEFT JOIN emp3 e ON e.did = d.id ORDER BY d.id, e.id;
SELECT count(*) FROM emp3, dept3;
SELECT count(*), count(pay), sum(pay), min(name), max(pay) FROM emp3;
SELECT avg(pay::float8) FROM emp3;
SELECT d.name, count(*) FROM emp3 e JOIN dept3 d ON e.did = d.id GROUP BY d.name ORDER BY d.name;
SELECT did, sum(pay) FROM emp3 GROUP BY did HAVING sum(pay) > 100 ORDER BY did;
SELECT name FROM emp3 WHERE pay > (SELECT avg(pay::float8) FROM emp3) ORDER BY name;
SELECT name FROM dept3 WHERE id IN (SELECT did FROM emp3) ORDER BY name;
SELECT name FROM dept3 WHERE id NOT IN (SELECT did FROM emp3) ORDER BY name;
SELECT name FROM dept3 WHERE id NOT IN (SELECT did FROM emp3 WHERE did IS NOT NULL) ORDER BY name;
DROP TABLE emp3;
DROP TABLE dept3;
-- derived tables and table functions with column-alias lists
SELECT id, name FROM (VALUES (1,'a'),(2,'b')) AS v(id,name) ORDER BY id;
SELECT id FROM (VALUES (1),(2),(3)) AS v(id) WHERE id > 1 ORDER BY id;
SELECT a + b AS s FROM (SELECT 10, 20) AS v(a, b);
SELECT y FROM (SELECT 1 AS x) AS v(y);
SELECT sum(n) FROM (VALUES (10),(20),(30)) AS t(n);
SELECT x FROM generate_series(1, 3) AS g(x) ORDER BY x;
SELECT * FROM (VALUES (1, 2)) AS v(a, b, c);
SELECT * FROM generate_series(1, 3) AS g(x, y);
-- aggregate FILTER (WHERE ...)
SELECT count(*) FILTER (WHERE x > 1), sum(x) FILTER (WHERE x < 3) FROM generate_series(1,3) AS g(x);
SELECT sum(x), sum(x) FILTER (WHERE x % 2 = 0) FROM generate_series(1,5) AS g(x);
SELECT count(*), count(*) FILTER (WHERE false) FROM generate_series(1,3) AS g(x);
SELECT string_agg(x::text, ',') FILTER (WHERE x <> 2) FROM generate_series(1,3) AS g(x);
-- value/positional window functions
SELECT x, ntile(3) OVER (ORDER BY x) FROM generate_series(1, 10) AS g(x) ORDER BY x;
SELECT x, first_value(x) OVER (ORDER BY x), last_value(x) OVER () FROM generate_series(1, 4) AS g(x) ORDER BY x;
SELECT x, nth_value(x, 2) OVER (ORDER BY x) FROM generate_series(1, 4) AS g(x) ORDER BY x;
SELECT x, first_value(x * 10) OVER (PARTITION BY x % 2 ORDER BY x) FROM generate_series(1, 6) AS g(x) ORDER BY x;
-- ordered-set aggregates
SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY x), percentile_cont(0.25) WITHIN GROUP (ORDER BY x) FROM generate_series(1, 5) AS g(x);
SELECT percentile_disc(0.5) WITHIN GROUP (ORDER BY x), percentile_disc(0.9) WITHIN GROUP (ORDER BY x) FROM generate_series(1, 10) AS g(x);
SELECT mode() WITHIN GROUP (ORDER BY x) FROM (VALUES (3),(1),(1),(3),(3)) AS v(x);
-- GROUPING SETS / ROLLUP / CUBE and the GROUPING() function
SELECT a, b, sum(v) FROM (VALUES ('x','p',1),('x','q',2),('y','p',4),('y','q',8)) AS t(a,b,v) GROUP BY ROLLUP(a, b) ORDER BY a, b;
SELECT a, b, sum(v) FROM (VALUES ('x','p',1),('x','q',2),('y','p',4),('y','q',8)) AS t(a,b,v) GROUP BY CUBE(a, b) ORDER BY a, b;
SELECT a, b, sum(v) FROM (VALUES ('x','p',1),('x','q',2),('y','p',4),('y','q',8)) AS t(a,b,v) GROUP BY GROUPING SETS ((a,b),(a),()) ORDER BY a, b;
SELECT a, grouping(a), grouping(b), grouping(a,b), sum(v) FROM (VALUES ('x','p',1),('x','q',2),('y','p',4)) AS t(a,b,v) GROUP BY ROLLUP(a, b) ORDER BY a, b;
SELECT a, sum(v) FROM (VALUES ('x',1),('y',2)) AS t(a,v) GROUP BY GROUPING SETS ((a),()) ORDER BY a;
SELECT sum(v) FROM (VALUES (1),(2),(3)) AS t(v) GROUP BY (); 
SELECT a, b, count(*) FROM (VALUES (1,10),(1,20),(2,10)) AS t(a,b) GROUP BY GROUPING SETS (a, b) ORDER BY a, b;
SELECT a, b, c, sum(v) FROM (VALUES (1,1,1,5),(1,2,1,7),(2,1,2,9)) AS t(a,b,c,v) GROUP BY a, ROLLUP(b, c) ORDER BY a, b, c;
-- grouping by parenthesized scalar expressions (must not be read as group lists)
SELECT (v + 1) * 2 AS g, count(*) FROM (VALUES (1),(1),(3)) AS t(v) GROUP BY (v + 1) * 2 ORDER BY g;
SELECT (v), sum(v) FROM (VALUES (1),(2),(2)) AS t(v) GROUP BY (v) ORDER BY 1;
-- string_agg with DISTINCT and ORDER BY on the aggregated expression
SELECT string_agg(DISTINCT v, ',' ORDER BY v) FROM (VALUES ('b'),('a'),('b'),('c'),('a')) AS t(v);
SELECT string_agg(DISTINCT v, ',' ORDER BY v DESC) FROM (VALUES ('b'),('a'),('c'),('a')) AS t(v);
-- DISTINCT with a different sort key is an error
SELECT string_agg(DISTINCT v, ',' ORDER BY k) FROM (VALUES ('a', 1)) AS t(v, k);
