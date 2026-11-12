-- PREPARE analyses its query as PostgreSQL's parse analysis does: operators,
-- functions, literal inputs and conditions of the query, its derived tables,
-- joins, subqueries and a DML condition fail before anything runs. Expanding
-- a non-composite value with (expression).* names its type.
CREATE TABLE d259 (v text, n int);
INSERT INTO d259 VALUES ('1', 1), ('3', 3);
PREPARE p0 AS SELECT * FROM d259 WHERE v > 2;
PREPARE p1 AS SELECT * FROM (SELECT v FROM d259) s WHERE v > 2;
PREPARE p2 AS SELECT v FROM d259 GROUP BY v HAVING v > 2;
PREPARE p3 AS SELECT v FROM d259 ORDER BY v + 1;
PREPARE p4 AS SELECT v FROM d259 WHERE n > 'x';
PREPARE p5 AS SELECT * FROM d259 JOIN d259 e ON d259.v = e.n;
PREPARE p6 AS SELECT v + 1 FROM d259;
PREPARE p8 AS SELECT * FROM d259 WHERE EXISTS (SELECT 1 FROM d259 e WHERE e.v > 2);
PREPARE p9 AS UPDATE d259 SET n = 1 WHERE v > 2;
PREPARE p10 AS DELETE FROM d259 WHERE v > 2;
PREPARE p11 AS SELECT * FROM d259 WHERE lower(v) > 2;
PREPARE p12 AS SELECT * FROM d259 WHERE nosuch(v);
PREPARE p13 AS SELECT * FROM d259 WHERE v = 'a' UNION ALL SELECT * FROM d259 WHERE n = 'b';
PREPARE ok1 AS SELECT * FROM d259 WHERE n > 2 ORDER BY v;
EXECUTE ok1;
CREATE FUNCTION f259(int) RETURNS int LANGUAGE sql AS 'SELECT $1';
CREATE FUNCTION f259(text) RETURNS text LANGUAGE sql AS 'SELECT $1';
SELECT (f259(d259.v)).* FROM d259;
SELECT (f259(1)).*;
SELECT (lower('a')).*;
SELECT (d259.v).* FROM d259;
SELECT (ARRAY[1]).*;
-- CREATE VIEW analyses its query the same way; a policy expression is
-- analysed without a position.
CREATE VIEW v259a AS SELECT * FROM d259 WHERE v > 2;
CREATE VIEW v259b AS SELECT v FROM d259 WHERE n = 'x';
CREATE VIEW v259c AS SELECT v FROM d259 ORDER BY v + 1;
CREATE VIEW v259d AS SELECT * FROM d259 WHERE EXISTS (SELECT 1 FROM d259 e WHERE e.v > 2);
CREATE VIEW v259e AS SELECT v FROM d259 WHERE n > 1;
CREATE POLICY p259 ON d259 USING (v > 2);
CREATE POLICY p259 ON d259 USING (nosuch > 0);
CREATE POLICY p259 ON d259 USING (n > 0);
