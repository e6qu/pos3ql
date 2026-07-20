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
-- USING and NATURAL joins: merged output columns, ordering, resolution
CREATE TABLE ju_a (id int, x text, k int);
CREATE TABLE ju_b (id int, y text, k int);
CREATE TABLE ju_c (z text);
CREATE TABLE ju_d (k int, w text);
INSERT INTO ju_a VALUES (1,'a1',10),(2,'a2',20);
INSERT INTO ju_b VALUES (1,'b1',10),(2,'b2',99),(3,'b3',30);
INSERT INTO ju_c VALUES ('c1');
INSERT INTO ju_d VALUES (10,'d1'),(99,'d2');
SELECT * FROM ju_a NATURAL JOIN ju_b;
SELECT * FROM ju_a NATURAL JOIN ju_c;
SELECT * FROM ju_b NATURAL LEFT JOIN ju_a;
SELECT ju_a.id, ju_b.id, id FROM ju_a NATURAL JOIN ju_b;
SELECT * FROM ju_a NATURAL FULL JOIN ju_b ORDER BY id, k;
SELECT * FROM ju_a NATURAL RIGHT JOIN ju_b ORDER BY id;
SELECT * FROM ju_a NATURAL INNER JOIN ju_b;
SELECT * FROM ju_a JOIN ju_b USING (id);
SELECT id FROM ju_a JOIN ju_b USING (id) ORDER BY id;
SELECT * FROM ju_a JOIN ju_b USING (id, k) JOIN ju_d USING (k);
SELECT * FROM ju_a a JOIN ju_b b USING (id) WHERE a.id = 1;
SELECT k, ju_a.k, ju_b.k FROM ju_a FULL JOIN ju_b USING (k) ORDER BY 1;
SELECT * FROM ju_a JOIN ju_b USING (id) JOIN ju_d ON ju_d.k = ju_a.k;
SELECT * FROM ju_b JOIN ju_a USING (k, id);
SELECT count(*) FROM ju_a NATURAL JOIN ju_b WHERE k = 10;
SELECT * FROM ju_a JOIN ju_b USING (id) WHERE k = 10;
SELECT id, k FROM ju_a NATURAL FULL JOIN ju_b WHERE k > 5 ORDER BY 1, 2;
-- error cases: ambiguous left column, missing right column
SELECT * FROM ju_a JOIN ju_b USING (id) JOIN ju_d USING (k);
SELECT * FROM ju_a JOIN ju_d USING (id);
SELECT * FROM ju_a JOIN ju_b USING (id) NATURAL JOIN ju_d;
DROP TABLE ju_a; DROP TABLE ju_b; DROP TABLE ju_c; DROP TABLE ju_d;
-- qualified star (t.*) and ORDER BY ordinals through star expansion
CREATE TABLE ts_a (id int, x text, k int);
CREATE TABLE ts_b (id int, y text);
INSERT INTO ts_a VALUES (2,'a2',20),(1,'a1',10);
INSERT INTO ts_b VALUES (1,'b1');
SELECT ts_a.* FROM ts_a ORDER BY id;
SELECT ts_a.*, x FROM ts_a ORDER BY id;
SELECT t.* FROM ts_a t ORDER BY id;
SELECT ts_a.* FROM ts_a t;
SELECT nosuch.* FROM ts_a;
SELECT ts_a.*, ts_b.* FROM ts_a JOIN ts_b USING (id);
SELECT * FROM ts_a ORDER BY 2;
SELECT ts_a.* FROM ts_a ORDER BY 3, 1;
SELECT * FROM ts_a ORDER BY 4;
SELECT x, ts_a.* FROM ts_a ORDER BY 2;
INSERT INTO ts_b VALUES (5,'e') RETURNING ts_b.*;
UPDATE ts_b SET y = 'q' WHERE id = 5 RETURNING ts_b.*;
DELETE FROM ts_b WHERE id = 5 RETURNING ts_b.*;
SELECT (SELECT t.* FROM (SELECT 42) t) AS s;
SELECT * FROM ts_a JOIN ts_b USING (id) ORDER BY 1;
DROP TABLE ts_a; DROP TABLE ts_b;
-- GROUP BY / HAVING / DISTINCT inside subqueries and EXISTS
CREATE TABLE gs1 (a int, b int);
CREATE TABLE gs2 (x int);
INSERT INTO gs1 VALUES (1,10),(1,20),(2,30),(2,40),(3,5);
INSERT INTO gs2 VALUES (30),(70),(99);
SELECT x FROM gs2 WHERE x IN (SELECT sum(b) FROM gs1 GROUP BY a) ORDER BY x;
SELECT x FROM gs2 WHERE x IN (SELECT DISTINCT b FROM gs1) ORDER BY x;
SELECT (SELECT sum(b) FROM gs1 GROUP BY a HAVING a = 2) AS s;
SELECT (SELECT DISTINCT a FROM gs1 WHERE a = 3) AS d;
SELECT EXISTS (SELECT a FROM gs1 GROUP BY a HAVING sum(b) > 60);
SELECT EXISTS (SELECT a FROM gs1 GROUP BY a HAVING sum(b) > 999);
SELECT EXISTS (SELECT DISTINCT a FROM gs1 WHERE a > 10);
SELECT (SELECT sum(b) FROM gs1 GROUP BY a) AS boom;
SELECT (SELECT count(*) FROM gs1 GROUP BY a ORDER BY a LIMIT 1) AS first_group;
SELECT (SELECT count(*) FROM gs1 GROUP BY a ORDER BY a LIMIT 1 OFFSET 2) AS third_group;
SELECT array(SELECT sum(b) FROM gs1 GROUP BY a ORDER BY a);
SELECT (SELECT count(*) FROM gs1) AS c, (SELECT max(b) FROM gs1) AS m;
DROP TABLE gs1; DROP TABLE gs2;
-- DISTINCT over grouped output; FROM-less aggregates; FROM-less WHERE in subqueries
CREATE TABLE dg (a int, b int);
INSERT INTO dg VALUES (1,10),(1,10),(2,10),(2,30);
SELECT count(*);
SELECT sum(3), max(7);
SELECT count(*) WHERE false;
SELECT count(*) HAVING count(*) > 5;
SELECT 1 HAVING true;
SELECT * FROM (SELECT count(*) AS c) t;
SELECT * FROM (SELECT sum(2) AS s WHERE 1 = 0) t;
SELECT DISTINCT count(*) FROM dg;
SELECT DISTINCT sum(b) FROM dg GROUP BY a ORDER BY 1;
SELECT * FROM (SELECT DISTINCT sum(b) AS s FROM dg GROUP BY a) t ORDER BY s;
SELECT x FROM (SELECT DISTINCT max(b) AS x FROM dg GROUP BY a) q ORDER BY x;
SELECT (SELECT count(*)) AS one;
SELECT (SELECT 5 WHERE false) AS v;
SELECT (SELECT sum(9) WHERE false) AS s;
SELECT EXISTS (SELECT count(*) WHERE false);
SELECT EXISTS (SELECT count(*) HAVING count(*) > 3);
SELECT sum(x.n) FROM (SELECT 1 AS n UNION ALL SELECT 2) x;
DROP TABLE dg;
-- correlated subqueries in WHERE of grouped and windowed outer queries
CREATE TABLE ct (k int, v int);
CREATE TABLE cu (k int, w int);
INSERT INTO ct VALUES (1,10),(1,20),(2,30),(2,40),(3,50);
INSERT INTO cu VALUES (1,100),(2,15),(3,60);
SELECT k, sum(v) FROM ct WHERE v < (SELECT w FROM cu WHERE cu.k = ct.k) GROUP BY k ORDER BY k;
SELECT count(*) FROM ct WHERE EXISTS (SELECT 1 FROM cu WHERE cu.k = ct.k AND cu.w > 50);
SELECT max(v) FROM ct WHERE ct.v > (SELECT min(w) FROM cu WHERE cu.k = ct.k);
SELECT k, row_number() OVER (ORDER BY v) FROM ct WHERE v < (SELECT w FROM cu WHERE cu.k = ct.k) ORDER BY 2;
SELECT k, v FROM ct WHERE v = (SELECT max(v) FROM ct t2 WHERE t2.k = ct.k) ORDER BY k;
SELECT k FROM ct GROUP BY k HAVING sum(v) > (SELECT avg(w) FROM cu) ORDER BY k;
DROP TABLE ct; DROP TABLE cu;
-- ANY / ALL / SOME quantified comparisons over subqueries
DROP TABLE IF EXISTS qa; DROP TABLE IF EXISTS qb;
CREATE TABLE qa (x int);
CREATE TABLE qb (y int);
INSERT INTO qa VALUES (1),(5),(9);
INSERT INTO qb VALUES (3),(7);
SELECT x FROM qa WHERE x = ANY (SELECT y FROM qb) ORDER BY x;
SELECT x FROM qa WHERE x > ANY (SELECT y FROM qb) ORDER BY x;
SELECT x FROM qa WHERE x > ALL (SELECT y FROM qb) ORDER BY x;
SELECT x FROM qa WHERE x <> ALL (SELECT y FROM qb) ORDER BY x;
SELECT x FROM qa WHERE x < SOME (SELECT y FROM qb) ORDER BY x;
SELECT x FROM qa WHERE x = ANY (SELECT y FROM qb WHERE false) ORDER BY x;
SELECT x FROM qa WHERE x > ALL (SELECT y FROM qb WHERE false) ORDER BY x;
SELECT 3 = ANY (SELECT y FROM qb);
SELECT 4 = ANY (SELECT y FROM qb);
SELECT 4 = ANY (SELECT NULL::int UNION SELECT 3);
SELECT 1 > ALL (SELECT NULL::int UNION ALL SELECT 0);
SELECT x FROM qa WHERE x >= ANY (SELECT max(y) FROM qb GROUP BY y) ORDER BY x;
SELECT x FROM qa WHERE x = ANY (SELECT y + qa.x - qa.x FROM qb) ORDER BY x;
DROP TABLE qa; DROP TABLE qb;
-- explicit window frames: ROWS / RANGE / GROUPS
DROP TABLE IF EXISTS wf;
CREATE TABLE wf (g text, v int);
INSERT INTO wf VALUES ('a',1),('a',2),('a',2),('a',4),('b',10),('b',20),('b',30);
SELECT g, v, sum(v) OVER (PARTITION BY g ORDER BY v ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM wf ORDER BY g, v;
SELECT g, v, sum(v) OVER (PARTITION BY g ORDER BY v ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM wf ORDER BY g, v;
SELECT g, v, sum(v) OVER (ORDER BY v ROWS 2 PRECEDING) FROM wf ORDER BY v, g;
SELECT g, v, sum(v) OVER (PARTITION BY g ORDER BY v RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM wf ORDER BY g, v;
SELECT g, v, sum(v) OVER (PARTITION BY g ORDER BY v RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM wf ORDER BY g, v;
SELECT g, v, sum(v) OVER (PARTITION BY g ORDER BY v DESC RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM wf ORDER BY g, v;
SELECT g, v, sum(v) OVER (PARTITION BY g ORDER BY v GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM wf ORDER BY g, v;
SELECT g, v, count(*) OVER (PARTITION BY g ORDER BY v ROWS BETWEEN 1 FOLLOWING AND 2 FOLLOWING) FROM wf ORDER BY g, v;
SELECT g, v, last_value(v) OVER (PARTITION BY g ORDER BY v ROWS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING) FROM wf ORDER BY g, v;
SELECT g, v, first_value(v) OVER (PARTITION BY g ORDER BY v ROWS BETWEEN 1 FOLLOWING AND 2 FOLLOWING) FROM wf ORDER BY g, v;
SELECT g, v, nth_value(v, 2) OVER (PARTITION BY g ORDER BY v ROWS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING) FROM wf ORDER BY g, v;
SELECT v, sum(v) OVER (ORDER BY v ROWS BETWEEN 1 PRECEDING AND 1 PRECEDING) FROM wf ORDER BY v, g;
SELECT v, sum(v) OVER (ORDER BY v ROWS -1 PRECEDING) FROM wf;
SELECT v, sum(v) OVER (ORDER BY v RANGE BETWEEN -1 PRECEDING AND CURRENT ROW) FROM wf;
SELECT v, sum(v) OVER (ORDER BY g, v RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM wf;
SELECT v, sum(v) OVER (GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM wf;
SELECT v, sum(v) OVER (ROWS BETWEEN CURRENT ROW AND 1 PRECEDING) FROM wf;
DROP TABLE wf;
-- window functions over grouped queries; DISTINCT/ORDER/LIMIT with windows
DROP TABLE IF EXISTS gw;
CREATE TABLE gw (g text, h text, v int);
INSERT INTO gw VALUES ('a','x',1),('a','y',2),('a','y',3),('b','x',10),('b','y',20),('c','x',5);
SELECT g, sum(v), rank() OVER (ORDER BY sum(v) DESC) FROM gw GROUP BY g ORDER BY g;
SELECT g, sum(v), sum(sum(v)) OVER (ORDER BY g) FROM gw GROUP BY g ORDER BY g;
SELECT g, h, sum(v), row_number() OVER (PARTITION BY g ORDER BY sum(v)) FROM gw GROUP BY g, h ORDER BY g, h;
SELECT g, count(*), lag(count(*)) OVER (ORDER BY g) FROM gw GROUP BY g ORDER BY g;
SELECT g, sum(v), rank() OVER (ORDER BY sum(v)) FROM gw GROUP BY g HAVING sum(v) > 5 ORDER BY g;
SELECT sum(v), row_number() OVER () FROM gw;
SELECT g || '!', sum(v) + 1, max(v) - min(v), dense_rank() OVER (ORDER BY sum(v) + 1) FROM gw GROUP BY g ORDER BY 1;
SELECT DISTINCT v % 2, rank() OVER (ORDER BY v % 2) FROM gw ORDER BY 1;
SELECT * FROM (SELECT v, row_number() OVER (ORDER BY v) AS r FROM gw ORDER BY v LIMIT 2) t;
SELECT count(*) FROM (SELECT DISTINCT v % 2 AS p, rank() OVER (ORDER BY v % 2) FROM gw) t;
SELECT g, sum(v), sum(sum(v)) OVER (ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM gw GROUP BY g ORDER BY g;
SELECT v, rank() OVER (ORDER BY nosuch) FROM gw GROUP BY v;
DROP TABLE gw;
-- RIGHT/FULL JOIN in any chain position
DROP TABLE IF EXISTS mj1; DROP TABLE IF EXISTS mj2; DROP TABLE IF EXISTS mj3; DROP TABLE IF EXISTS mj4;
CREATE TABLE mj1 (a int); CREATE TABLE mj2 (b int); CREATE TABLE mj3 (c int); CREATE TABLE mj4 (d int);
INSERT INTO mj1 VALUES (1),(2);
INSERT INTO mj2 VALUES (2),(3);
INSERT INTO mj3 VALUES (3),(4);
INSERT INTO mj4 VALUES (4),(5);
SELECT * FROM mj1 FULL JOIN mj2 ON a = b JOIN mj3 ON b = c ORDER BY 1,2,3;
SELECT * FROM mj1 RIGHT JOIN mj2 ON a = b LEFT JOIN mj3 ON b = c ORDER BY 1,2,3;
SELECT * FROM mj1 FULL JOIN mj2 ON a = b FULL JOIN mj3 ON b = c ORDER BY 1,2,3;
SELECT * FROM mj1 RIGHT JOIN mj2 ON a = b RIGHT JOIN mj3 ON b = c ORDER BY 1,2,3;
SELECT * FROM mj1 FULL JOIN mj2 ON a = b FULL JOIN mj3 ON b = c FULL JOIN mj4 ON c = d ORDER BY 1,2,3,4;
SELECT * FROM mj1 RIGHT JOIN mj2 ON a = b JOIN mj3 ON b = c RIGHT JOIN mj4 ON c = d ORDER BY 1,2,3,4;
SELECT * FROM mj1 FULL JOIN mj2 ON a = b LEFT JOIN mj3 ON b = c JOIN mj4 ON c = d ORDER BY 1,2,3,4;
SELECT count(*) FROM mj1 FULL JOIN mj2 ON a = b FULL JOIN mj3 ON b = c;
SELECT * FROM mj1 FULL JOIN mj2 ON a = b JOIN mj3 ON c = coalesce(b, 3) ORDER BY 1,2,3;
DROP TABLE mj1; DROP TABLE mj2; DROP TABLE mj3; DROP TABLE mj4;
-- set-returning functions with aggregates / DISTINCT / ORDER BY / LIMIT
DROP TABLE IF EXISTS st;
CREATE TABLE st (g text, v int);
INSERT INTO st VALUES ('a',1),('a',2),('b',10);
SELECT count(*), generate_series(1,2);
SELECT sum(x), generate_series(1,2) FROM (VALUES (5),(6)) v(x);
SELECT DISTINCT generate_series(1,3) % 2 ORDER BY 1;
SELECT generate_series(1,3) ORDER BY 1 DESC;
SELECT generate_series(1,3) AS n ORDER BY n DESC;
SELECT g, generate_series(1,2) FROM st ORDER BY g DESC, 2;
SELECT DISTINCT g, generate_series(1,2) AS s2 FROM st ORDER BY g, s2;
SELECT g, sum(v), generate_series(1,2) FROM st GROUP BY g ORDER BY g;
SELECT v, generate_series(1,2) FROM st ORDER BY v LIMIT 3;
SELECT * FROM (SELECT generate_series(1,3) AS n) t ORDER BY n DESC;
SELECT unnest(ARRAY[3,1,2]) ORDER BY 1;
DROP TABLE st;
CREATE TABLE st (g text, v int);
INSERT INTO st VALUES ('a',1),('a',2),('b',10);
SELECT generate_series(1,5) LIMIT 2;
SELECT generate_series(1,5) ORDER BY 1 DESC LIMIT 2 OFFSET 1;
SELECT DISTINCT g, generate_series(1,2) AS s FROM st ORDER BY g, s;
SELECT g, sum(v), generate_series(1,2) FROM st GROUP BY g HAVING sum(v) > 2 ORDER BY g, 3;
SELECT count(*), generate_series(1,2) ORDER BY 2 DESC;
DROP TABLE st;
-- window frames EXCLUDE + windows over grouping sets
DROP TABLE IF EXISTS ex;
CREATE TABLE ex (v int);
INSERT INTO ex VALUES (1),(2),(2),(3);
SELECT v, sum(v) OVER (ORDER BY v ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE CURRENT ROW) FROM ex ORDER BY v;
SELECT v, sum(v) OVER (ORDER BY v RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE GROUP) FROM ex ORDER BY v;
SELECT v, sum(v) OVER (ORDER BY v RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE TIES) FROM ex ORDER BY v;
SELECT v, sum(v) OVER (ORDER BY v ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE CURRENT ROW) FROM ex ORDER BY v;
SELECT v, count(*) OVER (ORDER BY v ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE GROUP) FROM ex ORDER BY v;
SELECT v, first_value(v) OVER (ORDER BY v ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE CURRENT ROW) FROM ex ORDER BY v;
SELECT v, last_value(v) OVER (ORDER BY v ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE TIES) FROM ex ORDER BY v;
SELECT v, nth_value(v, 2) OVER (ORDER BY v ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE GROUP) FROM ex ORDER BY v;
SELECT v, sum(v) OVER (EXCLUDE CURRENT ROW) FROM ex;
DROP TABLE ex;
DROP TABLE IF EXISTS gw2;
CREATE TABLE gw2 (a text, b text, v int);
INSERT INTO gw2 VALUES ('x','p',1),('x','q',2),('y','p',4),('y','q',8);
SELECT a, b, sum(v), rank() OVER (ORDER BY sum(v)) FROM gw2 GROUP BY ROLLUP(a, b) ORDER BY a, b, 4;
SELECT a, grouping(a) AS ga, sum(v), row_number() OVER (ORDER BY grouping(a), a) FROM gw2 GROUP BY ROLLUP(a) ORDER BY 4;
SELECT a, b, sum(v), sum(sum(v)) OVER (PARTITION BY grouping(a, b)) FROM gw2 GROUP BY GROUPING SETS ((a,b),(a),()) ORDER BY a, b, 3;
DROP TABLE gw2;
-- whole-row count(t.*); per-group and inner-grouped correlated subqueries
DROP TABLE IF EXISTS wr1; DROP TABLE IF EXISTS wr2;
CREATE TABLE wr1 (a int); CREATE TABLE wr2 (b int);
INSERT INTO wr1 VALUES (1),(2);
INSERT INTO wr2 VALUES (2);
SELECT count(wr2.*) FROM wr1 LEFT JOIN wr2 ON a = b;
SELECT count(wr1.*) FROM wr1;
SELECT count(w.*) FROM wr1 w;
SELECT count(wr1.*) FROM wr1 FULL JOIN wr2 ON a = b;
SELECT a, count(wr2.*) FROM wr1 LEFT JOIN wr2 ON a = b GROUP BY a ORDER BY a;
DROP TABLE wr1; DROP TABLE wr2;
DROP TABLE IF EXISTS c1; DROP TABLE IF EXISTS c2;
CREATE TABLE c1 (k int, v int);
CREATE TABLE c2 (k int, w int);
INSERT INTO c1 VALUES (1,10),(1,20),(2,30),(2,40),(3,50);
INSERT INTO c2 VALUES (1,100),(2,15),(3,60),(3,5);
-- correlated in the select list of a grouped query (per group)
SELECT k, sum(v), (SELECT max(w) FROM c2 WHERE c2.k = c1.k) FROM c1 GROUP BY k ORDER BY k;
-- correlated in HAVING (per group)
SELECT k FROM c1 GROUP BY k HAVING sum(v) > (SELECT min(w) FROM c2 WHERE c2.k = c1.k) ORDER BY k;
-- correlated in an aggregate argument (per input row)
SELECT k, sum(v + (SELECT count(*) FROM c2 WHERE c2.k = c1.k)) FROM c1 GROUP BY k ORDER BY k;
-- correlated EXISTS in the select list of a grouped query
SELECT k, EXISTS (SELECT 1 FROM c2 WHERE c2.k = c1.k AND c2.w > 50) FROM c1 GROUP BY k ORDER BY k;
-- ungrouped outer column in a grouped-query subquery errors
SELECT k, (SELECT max(w) FROM c2 WHERE c2.w = c1.v) FROM c1 GROUP BY k ORDER BY k;
DROP TABLE c1; DROP TABLE c2;
CREATE TABLE c1 (k int, v int);
CREATE TABLE c2 (k int, w int);
INSERT INTO c1 VALUES (1,10),(1,20),(2,30),(2,40),(3,50);
INSERT INTO c2 VALUES (1,100),(2,15),(3,60),(3,5);
-- inner query groups over the outer reference
SELECT k, v FROM c1 WHERE v IN (SELECT max(w) FROM c2 WHERE c2.k = c1.k GROUP BY c2.k) ORDER BY k;
SELECT k, (SELECT sum(w) FROM c2 WHERE c2.k = c1.k GROUP BY c2.k) FROM c1 GROUP BY k ORDER BY k;
SELECT count(*) FROM c1 WHERE EXISTS (SELECT c2.k FROM c2 WHERE c2.k = c1.k GROUP BY c2.k HAVING sum(w) > 50);
SELECT k, v FROM c1 WHERE v > ALL (SELECT DISTINCT w FROM c2 WHERE c2.k = c1.k) ORDER BY k, v;
DROP TABLE c1; DROP TABLE c2;
-- records: t.*, (t.*), bare table, ROW(), row_to_json/to_json/to_jsonb, record compare, RETURNING
DROP TABLE IF EXISTS rec; DROP TABLE IF EXISTS q;
CREATE TABLE rec (a int, b text, c bool);
INSERT INTO rec VALUES (1,'x',true),(2,'y',false),(2,'z',NULL);
SELECT row_to_json(rec.*) FROM rec ORDER BY a, b;
SELECT to_jsonb(rec.*) FROM rec ORDER BY a, b;
SELECT to_json(rec.*) FROM rec ORDER BY a, b;
SELECT rec.* FROM rec ORDER BY a, b;
SELECT (rec.*) FROM rec ORDER BY a, b;
SELECT rec FROM rec ORDER BY a, b;
SELECT row_to_json(rec) FROM rec ORDER BY a, b;
SELECT max(rec.*) FROM rec;
SELECT min(rec) FROM rec;
SELECT row_to_json(row(1, 'hi', true));
SELECT to_jsonb(row(1, 'hi', true));
SELECT row(1, 'a', NULL);
SELECT (rec)::text FROM rec ORDER BY a, b;
SELECT row_to_json(r) FROM (SELECT a, b FROM rec) r ORDER BY a, b;
SELECT r FROM (SELECT a, b FROM rec) r ORDER BY a, b;
SELECT rec.a, rec FROM rec ORDER BY a, b;
DROP TABLE rec;
CREATE TABLE q (a text, b int);
INSERT INTO q VALUES ('hi, there', 1), ('quote"d', 2), ('back\slash', 3), ('', 4), (NULL, 5), ('has ) paren', 6);
SELECT q FROM q ORDER BY b;
SELECT row_to_json(q) FROM q ORDER BY b;
SELECT count(q.*) FROM q;
SELECT q.* FROM q ORDER BY b;
SELECT (q.*) FROM q ORDER BY b;
DROP TABLE q;
DROP TABLE IF EXISTS ins;
CREATE TABLE ins (a int, b text);
INSERT INTO ins VALUES (1,'x') RETURNING row_to_json(ins.*);
INSERT INTO ins VALUES (2,'y') RETURNING ins;
UPDATE ins SET b = 'z' WHERE a = 1 RETURNING to_jsonb(ins.*), ins;
DELETE FROM ins WHERE a = 2 RETURNING ins.*;
DROP TABLE ins;
-- correlated subqueries in the select list of a window-function query
DROP TABLE IF EXISTS w1; DROP TABLE IF EXISTS w2;
CREATE TABLE w1 (k int, v int);
CREATE TABLE w2 (k int, w int);
INSERT INTO w1 VALUES (1,10),(1,20),(2,30);
INSERT INTO w2 VALUES (1,100),(2,5);
SELECT k, v, row_number() OVER (ORDER BY v), (SELECT max(w) FROM w2 WHERE w2.k = w1.k) FROM w1 ORDER BY v;
SELECT k, sum(v) OVER (PARTITION BY k), (SELECT count(*) FROM w2 WHERE w2.w > w1.v) FROM w1 ORDER BY k, v;
SELECT k, rank() OVER (ORDER BY v DESC) FROM w1 WHERE EXISTS (SELECT 1 FROM w2 WHERE w2.k = w1.k) ORDER BY v;
SELECT DISTINCT k, (SELECT max(w) FROM w2 WHERE w2.k = w1.k) AS m, count(*) OVER () FROM w1 ORDER BY k;
DROP TABLE w1; DROP TABLE w2;
-- correlated subqueries in ORDER BY and in derived-table select lists
DROP TABLE IF EXISTS m1; DROP TABLE IF EXISTS m2;
CREATE TABLE m1 (k int, v int); CREATE TABLE m2 (k int, w int);
INSERT INTO m1 VALUES (1,10),(2,20),(1,30); INSERT INTO m2 VALUES (1,5),(2,6);
SELECT k FROM m1 ORDER BY (SELECT max(w) FROM m2 WHERE m2.k = m1.k) DESC, k LIMIT 2;
SELECT * FROM (SELECT k, (SELECT w FROM m2 WHERE m2.k = m1.k) AS s FROM m1) t WHERE s > 5 ORDER BY k;
SELECT DISTINCT (SELECT max(w) FROM m2 WHERE m2.k = m1.k) FROM m1 ORDER BY 1;
SELECT t.s + 1 FROM (SELECT (SELECT w FROM m2 WHERE m2.k = m1.k) AS s FROM m1) t ORDER BY 1;
DROP TABLE m1; DROP TABLE m2;
-- DISTINCT ON (exprs)
DROP TABLE IF EXISTS d;
CREATE TABLE d (k int, v int, t text);
INSERT INTO d VALUES (1,10,'a'),(1,20,'b'),(2,5,'c'),(2,15,'d'),(1,10,'e');
SELECT DISTINCT ON (k) k, v, t FROM d ORDER BY k, v;
SELECT DISTINCT ON (k) k, v FROM d ORDER BY k, v DESC;
SELECT DISTINCT ON (k, v) k, v, t FROM d ORDER BY k, v, t;
SELECT DISTINCT ON (k) k, v FROM d;
SELECT DISTINCT ON (v) k FROM d ORDER BY v;
SELECT DISTINCT ON (k) k, v FROM d ORDER BY k, v LIMIT 1;
SELECT DISTINCT ON (k) k FROM d ORDER BY k DESC;
DROP TABLE d;
-- DISTINCT ON must match initial ORDER BY
DROP TABLE IF EXISTS d; CREATE TABLE d(k int,v int); INSERT INTO d VALUES (1,2),(2,1);
SELECT DISTINCT ON (k) k FROM d ORDER BY v;
SELECT DISTINCT ON (k) k FROM d ORDER BY v, k;
SELECT DISTINCT ON (k) k, v FROM d ORDER BY k, v;
DROP TABLE d;
-- array_agg (ORDER BY, DISTINCT, FILTER, NULLs); untyped NULL in VALUES/UNION; UNION ALL order
SELECT array_agg(k) FROM (VALUES (1),(3),(2)) t(k);
SELECT array_agg(k ORDER BY k DESC) FROM (VALUES (1),(3),(2)) t(k);
SELECT array_agg(DISTINCT k) FROM (VALUES (1),(1),(2)) t(k);
SELECT array_agg(t) FROM (VALUES ('a'),('b')) t(t);
SELECT array_agg(k) FROM (VALUES (1)) t(k) WHERE k > 5;
SELECT g, array_agg(v ORDER BY v) FROM (VALUES ('x',2),('x',1),('y',3)) t(g,v) GROUP BY g ORDER BY g;
SELECT array_agg(k) FROM (VALUES (1),(NULL),(2)) t(k);
SELECT array_agg(k ORDER BY k NULLS FIRST) FROM (VALUES (2),(NULL),(1)) t(k);
SELECT array_agg(k) FILTER (WHERE k > 1) FROM (VALUES (1),(2),(3)) t(k);
SELECT array_length(array_agg(k), 1) FROM (VALUES (1),(2)) t(k);
SELECT cardinality(array_agg(DISTINCT k ORDER BY k)) FROM (VALUES (3),(1),(1),(2)) t(k);
SELECT k FROM (VALUES (1),(NULL),(2)) t(k) ORDER BY k;
SELECT * FROM (VALUES ('a', 1),(NULL, 2),('c', NULL)) t(a,b) ORDER BY b;
SELECT 1 UNION SELECT NULL ORDER BY 1;
SELECT NULL UNION SELECT NULL;
SELECT a FROM (VALUES (NULL),(NULL)) t(a);
-- json_build_object/array, json_agg/jsonb_agg, json_object_agg
SELECT json_build_object('a', 1, 'b', 'x');
SELECT jsonb_build_object('a', 1, 'b', true, 'c', null);
SELECT json_build_array(1, 'x', true, null);
SELECT jsonb_build_array(1, 2.5, 'y');
SELECT json_agg(k) FROM (VALUES (1),(3),(2)) t(k);
SELECT jsonb_agg(k ORDER BY k DESC) FROM (VALUES (1),(3),(2)) t(k);
SELECT json_agg(row_to_json(t)) FROM (VALUES (1,'a'),(2,'b')) t(k,v);
SELECT json_object_agg(k, v) FROM (VALUES ('a',1),('b',2)) t(k,v);
SELECT jsonb_object_agg(k, v) FROM (VALUES ('a',1),('b',2)) t(k,v);
SELECT json_agg(k) FROM (VALUES (1)) t(k) WHERE k > 5;
SELECT g, json_agg(v ORDER BY v) FROM (VALUES ('x',2),('x',1),('y',3)) t(g,v) GROUP BY g ORDER BY g;
SELECT json_build_object('nested', json_build_array(1,2), 'obj', json_build_object('x',1));
SELECT jsonb_build_object('arr', ARRAY[1,2,3]);
SELECT json_agg(DISTINCT k ORDER BY k) FROM (VALUES (3),(1),(1),(2)) t(k);
-- DISTINCT ON in FROM-less, derived-table, set-op, and grouped contexts
SELECT DISTINCT ON (1) 1, 2;
SELECT DISTINCT ON (a%2) a FROM (VALUES (1),(3),(2),(4)) t(a) ORDER BY a%2, a;
(SELECT DISTINCT ON (a) a, b FROM (VALUES (1,2),(1,3)) t(a,b)) UNION ALL SELECT 9, 9;
SELECT count(*) FROM (SELECT DISTINCT ON (k) k FROM (VALUES (1),(1),(2)) t(k)) s;
SELECT DISTINCT ON (k) k, sum(v) FROM (VALUES (1,10),(1,20),(2,5)) t(k,v) GROUP BY k ORDER BY k;
-- jsonb object-key ordering (length then bytewise), duplicate handling
SELECT '{"b":1,"aa":2,"c":3}'::jsonb;
SELECT '{"a":1,"a":2}'::jsonb;
SELECT '{"name":1,"id":2,"a":3,"created_at":4}'::jsonb;
-- json preserves source order/whitespace/duplicates verbatim
SELECT '{"b":1,  "aa":2,"c":3}'::json;
SELECT '[1,  2,{"k":2}]'::json;
-- jsonb || merge, path and existence operators, typeof, extract_path
SELECT '{"a":1,"b":2}'::jsonb || '{"b":3,"c":4}'::jsonb;
SELECT '[1,2]'::jsonb || '[3,4]'::jsonb;
SELECT '{"a":{"b":{"c":5}}}'::jsonb #> '{a,b,c}';
SELECT '{"a":{"b":{"c":5}}}'::jsonb #>> '{a,b,c}';
SELECT '{"a":1,"b":2}'::jsonb ? 'a';
SELECT '{"a":1}'::jsonb ?| ARRAY['x','a'];
SELECT '{"a":1,"b":2}'::jsonb ?& ARRAY['a','b'];
SELECT jsonb_typeof('[1,2]'::jsonb), json_typeof('"x"'::json), jsonb_typeof('null'::jsonb);
SELECT jsonb_extract_path('{"a":{"b":5}}'::jsonb, 'a', 'b');
SELECT jsonb_extract_path_text('{"a":{"b":5}}'::jsonb, 'a', 'b');
-- ->> / #>> decode JSON string escapes to raw characters
SELECT ('{"k":"x\ty"}'::jsonb)->>'k';
SELECT ('{"k":"x\ty"}'::json)->>'k';
SELECT '"abc"'::jsonb #>> '{}';
-- object_keys / array_elements (json = source order, jsonb = normalized)
SELECT jsonb_object_keys('{"z":1,"a":2,"mm":3}'::jsonb);
SELECT json_object_keys('{"z":1,"a":2,"a":9}'::json);
SELECT jsonb_array_elements('[1,"x",{"k":2},null]'::jsonb);
SELECT json_array_elements('[ {"k":  2} , 3 ]'::json);
SELECT jsonb_array_elements_text('[ {"k":  2} ,"x\ty", 3 ]'::jsonb);
SELECT json_array_elements_text('["a b",true,null]'::json);
-- SRFs in FROM position, alias-as-scalar, headers, lateral cross product
SELECT * FROM json_array_elements('[1,2]'::json);
SELECT x FROM json_array_elements_text('["p","q"]') AS x;
SELECT elem->>'n' FROM json_array_elements('[{"n":1},{"n":2}]'::json) elem;
SELECT k FROM jsonb_object_keys('{"z":1,"a":2}'::jsonb) AS k;
SELECT g, e FROM generate_series(1,2) g, jsonb_array_elements('[9,8]'::jsonb) e ORDER BY g, e::text;
-- error messages for the wrong JSON shape
SELECT json_array_elements('5'::json);
SELECT json_array_elements('{"a":1}'::json);
SELECT jsonb_array_elements('{"a":1}'::jsonb);
SELECT json_object_keys('[1]'::json);
SELECT jsonb_object_keys('[1]'::jsonb);
-- json_each / jsonb_each[_text]: two-column (key, value) set-returning functions
SELECT * FROM json_each('{"a":1,"b":"x"}'::json);
SELECT * FROM jsonb_each('{"b":1,"aa":2}'::jsonb);
SELECT * FROM json_each_text('{"a":1,"b":"x\ty"}'::json);
SELECT * FROM jsonb_each_text('{"a":1,"b":"x\ty"}'::jsonb);
SELECT key, value FROM json_each('{"a":{"n":1},"b":[1,2]}'::json);
SELECT json_each('{"a":1}'::json);
SELECT * FROM json_each('{"a":1,"a":2}'::json);
SELECT * FROM jsonb_each('{"a":1,"a":2}'::jsonb);
SELECT * FROM json_each('{}'::json);
SELECT e.key FROM json_each('{"x":1,"y":2}'::json) e;
SELECT k, v FROM jsonb_each_text('{"m":true,"n":null}'::jsonb) AS t(k, v);
SELECT json_each('[1,2]'::json);
SELECT jsonb_each('5'::jsonb);
-- KEY is a non-reserved keyword: usable as a column name
SELECT key FROM (SELECT 1 AS key, 2 AS value) t;
-- composite field access (record).field and expansion (record).*
SELECT (ROW(1,2)).f1, (ROW(1,2)).f2;
SELECT (ROW(10,'x',true)).*;
SELECT (ROW(1,'hi',true)).f2;
SELECT (ROW(1,2)).f3;
DROP TABLE IF EXISTS rectest;
CREATE TABLE rectest(a int, b text, c bool);
INSERT INTO rectest VALUES (1,'x',true),(2,'y',false);
SELECT (rectest).a, (rectest).b FROM rectest ORDER BY 1;
SELECT (rectest.*).c FROM rectest ORDER BY a;
SELECT (rectest).* FROM rectest ORDER BY a;
SELECT (json_each('{"a":1,"b":2}'::json)).*;
SELECT (jsonb_each('{"b":1,"aa":2}'::jsonb)).key;
SELECT (json_each_text('{"k":"v"}'::json)).value;
SELECT (ROW(1,2,3)).*, 99 AS extra;
SELECT count(*) FROM (SELECT (json_each('{"a":1,"b":2,"c":3}'::json)).*) s;
DROP TABLE rectest;
-- _pg_expandarray field access (driver introspection): direct and via a
-- derived-table column with a qualified base (must resolve, not error)
SELECT (information_schema._pg_expandarray(ARRAY[10,20,30])).x, (information_schema._pg_expandarray(ARRAY[10,20,30])).n;
SELECT (result.keys).x AS col, (result.keys).n AS seq FROM (SELECT information_schema._pg_expandarray(ARRAY[7,8,9]) AS keys) result ORDER BY seq;
-- array manipulation functions (append/prepend/cat/remove/replace/dims/trim + to_json)
SELECT array_append(ARRAY[1,2], 3), array_prepend(0, ARRAY[1,2]);
SELECT array_append(NULL::int[], 5), array_append(ARRAY[1,2], NULL::int);
SELECT array_cat(ARRAY[1,2], ARRAY[3,4]), array_cat(ARRAY[]::int[], ARRAY[9]);
SELECT array_remove(ARRAY[1,2,2,3], 2), array_remove(ARRAY[1,NULL,2], NULL);
SELECT array_remove(ARRAY['a','b','a'], 'a');
SELECT array_replace(ARRAY[1,2,2,3], 2, 9), array_replace(ARRAY[1,NULL,2], NULL, 0);
SELECT array_ndims(ARRAY[1,2]), array_ndims(ARRAY[]::int[]);
SELECT array_dims(ARRAY[1,2,3]), array_dims(ARRAY[]::int[]);
SELECT trim_array(ARRAY[1,2,3,4], 1), trim_array(ARRAY[1,2,3], 3);
SELECT trim_array(ARRAY[1,2], 5);
SELECT array_to_json(ARRAY[1,2,3]), array_to_json(ARRAY['a','b']);
-- element-type promotion (polymorphic anyarray/anyelement)
SELECT array_append(ARRAY[1,2], 3.5), array_cat(ARRAY[1,2], ARRAY[3.5,4.5]);
SELECT pg_typeof(array_append(ARRAY[1,2], 3)), pg_typeof(array_append(ARRAY[1,2], 3.5));
-- array-to-array cast (element re-encode)
SELECT ARRAY[1,2]::int8[], ARRAY[1,2]::numeric[];
SELECT pg_typeof(ARRAY[1,2]::int8[]);
-- regex string functions: substring(FROM pattern), regexp_like, regexp_split_to_array
SELECT substring('foobar' from 'o+'), substring('foobar' from 'x+');
SELECT substring('foobar' from '(o+)(b)');
SELECT substring('XY1234Z' from '%#"[0-9]+#"%' for '#');
SELECT substring('abcXYZdef' from '%#"[A-Z]+#"%' for '#');
SELECT substring('foobar', 2, 3), substring('foobar' from 2 for 3), substring('foobar' from 2);
SELECT regexp_like('ABC', 'abc'), regexp_like('ABC', 'abc', 'i'), regexp_like('foobar', 'x+');
SELECT regexp_split_to_array('a,b,c', ','), regexp_split_to_array('the quick brown', '\s+');
SELECT regexp_split_to_array('abc', ''), regexp_split_to_array('a1b22c', '[0-9]+');
SELECT regexp_split_to_array('a,b,', ',');
SELECT pg_typeof(regexp_split_to_array('a,b', ',')), pg_typeof(regexp_like('a','a'));
-- regexp_split_to_table, generate_subscripts, and WITH ORDINALITY
SELECT regexp_split_to_table('a,b,c', ',');
SELECT v FROM regexp_split_to_table('a1b22c', '[0-9]+') AS s(v);
SELECT regexp_split_to_table('the quick brown', '\s+');
SELECT generate_subscripts(ARRAY[10,20,30], 1);
SELECT sub FROM generate_subscripts(ARRAY['a','b'], 1) AS g(sub) ORDER BY sub;
SELECT generate_subscripts(ARRAY[10,20], 2);
SELECT * FROM unnest(ARRAY['x','y','z']) WITH ORDINALITY;
SELECT elem, idx FROM unnest(ARRAY['x','y']) WITH ORDINALITY AS t(elem, idx) ORDER BY idx DESC;
SELECT * FROM generate_series(5,7) WITH ORDINALITY;
SELECT w, n FROM regexp_split_to_table('a,b,c', ',') WITH ORDINALITY AS t(w, n) ORDER BY n;
SELECT key, value, ordinality FROM json_each('{"a":1,"b":2}') WITH ORDINALITY ORDER BY ordinality;
SELECT sum(ordinality) FROM unnest(ARRAY[10,20,30]) WITH ORDINALITY;
-- generate_series over timestamps/dates, date_bin, and scalar temporal fns
SELECT generate_series('2024-01-01'::timestamp, '2024-01-01 03:00', '1 hour');
SELECT pg_typeof(generate_series('2024-01-01'::timestamp, '2024-01-01 03:00', '1 hour'));
SELECT generate_series('2024-01-01'::date, '2024-01-03'::date, '1 day');
SELECT pg_typeof(generate_series('2024-01-01'::date, '2024-01-03'::date, '1 day'));
SELECT generate_series('2024-01-01 00:00+00'::timestamptz, '2024-01-01 02:00+00', '1 hour');
SELECT generate_series('2024-01-03'::timestamp, '2024-01-01', '-1 day');
SELECT generate_series('2024-01-31'::timestamp, '2024-04-30', '1 month');
SELECT ts FROM generate_series('2024-01-01'::timestamp, '2024-01-01 02:00', '1 hour') AS ts;
SELECT * FROM generate_series('2024-01-01'::timestamp, '2024-01-01 02:00', '1 hour') WITH ORDINALITY;
SELECT count(*) FROM generate_series('2024-01-01'::timestamp, '2024-12-31', '1 day');
SELECT date_bin('15 min', TIMESTAMP '2024-01-01 00:17', TIMESTAMP '2024-01-01');
SELECT date_bin('2 hours', TIMESTAMP '2024-01-01 05:30', TIMESTAMP '2024-01-01');
SELECT make_timestamptz(2024,1,1,12,0,0);
SELECT isfinite(DATE '2024-01-01'), isfinite(TIMESTAMP '2024-01-01'), isfinite(INTERVAL '1 day');
SELECT generate_series('2024-01-01'::timestamp, '2024-01-02', '0 hour');
SELECT date_bin('1 month', TIMESTAMP '2024-06-15', TIMESTAMP '2024-01-01');
-- multiple set-returning functions in the select list run in lockstep to the
-- longest, shorter ones NULL-padding (PostgreSQL 10+ semantics)
SELECT generate_series(1,3), generate_series(1,2);
SELECT generate_series(1,2), generate_series(1,4);
SELECT generate_series(1,3) a, generate_series(10,40,10) b;
SELECT unnest(ARRAY[1,2,3]), unnest(ARRAY['a','b']);
SELECT generate_series(1,4), generate_series(1,2), generate_series(1,3);
SELECT generate_series('2024-01-01'::timestamp, '2024-01-03', '1 day'), generate_series(1,2);
SELECT generate_series(1,3), unnest(ARRAY['x','y','z','w']);
SELECT generate_series(5,1,-1), generate_series(1,2);
SELECT g, generate_series(1,2) FROM generate_series(1,2) g;
-- encode/decode, cryptographic hashes, bytea manipulation, quoting, OVERLAPS
SELECT encode('abc'::bytea, 'base64'), encode('abc'::bytea, 'hex');
SELECT encode('\x00ff41'::bytea, 'escape'), encode('\x00ff41'::bytea, 'base64');
SELECT decode('YWJj', 'base64'), decode('616263', 'hex'), decode('a\000b', 'escape');
SELECT 'abc'::bytea, '\x616263'::bytea, length('abcde'::bytea);
SELECT encode(sha224('abc'::bytea), 'hex');
SELECT encode(sha256('abc'::bytea), 'hex');
SELECT encode(sha384('abc'::bytea), 'hex');
SELECT encode(sha512('abc'::bytea), 'hex');
SELECT encode(convert_to('héllo', 'UTF8'), 'hex'), convert_from(convert_to('a€b','UTF8'), 'UTF8');
SELECT get_byte('abc'::bytea, 1), set_byte('abc'::bytea, 1, 90);
SELECT get_bit('\x02'::bytea, 1), set_bit('\x00'::bytea, 3, 1);
SELECT bit_count('abc'::bytea), bit_count('\xff0f'::bytea);
SELECT quote_ident('foo bar'), quote_ident('simple'), quote_ident('select'), quote_ident('UpperCase');
SELECT quote_literal('a''b'), quote_literal(42), quote_nullable(NULL::int), quote_nullable('x');
SELECT parse_ident('a.b.c'), parse_ident('public."My Table"');
SELECT (DATE '2024-01-01', DATE '2024-02-01') OVERLAPS (DATE '2024-01-15', DATE '2024-03-01');
SELECT (DATE '2024-01-01', DATE '2024-02-01') OVERLAPS (DATE '2024-02-01', DATE '2024-03-01');
SELECT (DATE '2024-01-01', INTERVAL '1 month') OVERLAPS (DATE '2024-01-15', DATE '2024-03-01');
SELECT (TIMESTAMP '2024-01-01 10:00', TIMESTAMP '2024-01-01 12:00') OVERLAPS (TIMESTAMP '2024-01-01 11:00', TIMESTAMP '2024-01-01 13:00');
-- jsonb manipulation: set/insert/strip_nulls/pretty, delete operators
SELECT jsonb_set('{"a":1,"b":2}', '{a}', '9'), jsonb_set('{"a":{"b":1}}', '{a,b}', '9');
SELECT jsonb_set('[0,1,2]', '{1}', '9'), jsonb_set('[0,1,2]', '{-1}', '9');
SELECT jsonb_set('{"a":1}', '{b}', '9', false), jsonb_set('{"a":1}', '{b}', '9', true);
SELECT jsonb_insert('{"a":[1,2]}', '{a,1}', '5'), jsonb_insert('{"a":[1,2]}', '{a,1}', '5', true);
SELECT jsonb_strip_nulls('{"a":1,"b":null,"c":{"d":null}}');
SELECT jsonb_pretty('{"a":1,"b":[1,2]}');
SELECT '{"a":1,"b":2}'::jsonb - 'b';
SELECT '[0,1,2,3]'::jsonb - 1, '[0,1,2,3]'::jsonb - -1;
SELECT '{"a":1,"b":2,"c":3}'::jsonb - ARRAY['a','c'];
SELECT '{"a":{"b":1,"c":2}}'::jsonb #- '{a,b}';
SELECT '{"a":1}'::jsonb || '{"b":2}', '{"a":1}'::jsonb || '{"a":5,"c":3}';
SELECT jsonb_set('{"x":1}'::jsonb, '{x}', to_jsonb('hi'::text));
-- statistical aggregates: exact numeric variance/stddev over integer/numeric,
-- float8 regression/correlation/covariance family (rounded for determinism)
SELECT variance(x), var_pop(x), stddev(x), stddev_pop(x) FROM (VALUES (2),(4),(6),(7)) t(x);
SELECT variance(x), stddev_samp(x) FROM (VALUES (1.5),(2.5),(3.5),(4.5)) t(x);
SELECT var_pop(x), stddev_pop(x) FROM (VALUES (5)) t(x);
SELECT var_samp(x), stddev_samp(x) FROM (VALUES (5)) t(x);
SELECT variance(x::float8), stddev(x::float8) FROM (VALUES (2),(4),(6)) t(x);
SELECT regr_count(y,x), regr_avgx(y,x), regr_avgy(y,x) FROM (VALUES (1,1),(2,3),(3,2),(4,5)) t(x,y);
SELECT round(regr_slope(y,x)::numeric,6), round(regr_intercept(y,x)::numeric,6) FROM (VALUES (1,1),(2,3),(3,2),(4,5)) t(x,y);
SELECT round(corr(y,x)::numeric,6), round(regr_r2(y,x)::numeric,6) FROM (VALUES (1,1),(2,3),(3,2),(4,5)) t(x,y);
SELECT round(covar_pop(y,x)::numeric,6), round(covar_samp(y,x)::numeric,6) FROM (VALUES (1,1),(2,3),(3,2),(4,5)) t(x,y);
SELECT round(regr_sxx(y,x)::numeric,6), round(regr_syy(y,x)::numeric,6), round(regr_sxy(y,x)::numeric,6) FROM (VALUES (1,1),(2,3),(3,2),(4,5)) t(x,y);
SELECT g, var_pop(x) FROM (VALUES ('a',1),('a',3),('b',10)) t(g,x) GROUP BY g ORDER BY g;
SELECT x, stddev(x) OVER (ORDER BY x) FROM (VALUES (2),(4),(6),(8)) t(x) ORDER BY x;
SELECT variance(DISTINCT x) FROM (VALUES (2),(2),(4),(6),(6)) t(x);
-- percent_rank / cume_dist window functions
SELECT x, round(percent_rank() OVER (ORDER BY x)::numeric,6), round(cume_dist() OVER (ORDER BY x)::numeric,6) FROM (VALUES (1),(2),(2),(3)) t(x) ORDER BY x;
SELECT g, x, round(percent_rank() OVER (PARTITION BY g ORDER BY x)::numeric,4), round(cume_dist() OVER (PARTITION BY g ORDER BY x)::numeric,4) FROM (VALUES ('a',1),('a',2),('b',5),('b',5),('b',9)) t(g,x) ORDER BY g,x;
-- pg_size_pretty (bigint): bytes/kB/MB/GB/TB/PB with half rounding
SELECT pg_size_pretty(0::bigint), pg_size_pretty(1023::bigint), pg_size_pretty(1536::bigint), pg_size_pretty(10240::bigint);
SELECT pg_size_pretty(5000000::bigint), pg_size_pretty(1073741824::bigint), pg_size_pretty(1099511627776::bigint), pg_size_pretty((-1536)::bigint);
SELECT pg_size_pretty(1125899906842624000::bigint), pg_size_pretty(9223372036854775807::bigint);
-- width_bucket over a thresholds array (binary search)
SELECT width_bucket(5, ARRAY[1,3,7,10]), width_bucket(0, ARRAY[1,3,7,10]), width_bucket(3, ARRAY[1,3,7,10]), width_bucket(100, ARRAY[1,3,7,10]);
SELECT width_bucket(5.5, ARRAY[1.0,3.0,7.0]), width_bucket('m', ARRAY['a','g','q']), width_bucket(3, ARRAY[]::int[]);
-- pg_typeof reports the static type of a NULL-valued argument
SELECT pg_typeof(NULL::integer), pg_typeof(NULL::numeric), pg_typeof(NULL::text), pg_typeof(NULL::int[]);
SELECT pg_typeof(max(x)) FROM (VALUES (1)) t(x) WHERE false;
SELECT pg_typeof(sum(x)) FROM (VALUES (1::bigint)) t(x) WHERE false;
