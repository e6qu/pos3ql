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
