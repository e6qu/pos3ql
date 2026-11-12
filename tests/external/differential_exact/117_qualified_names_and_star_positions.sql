-- A multiple-column assignment's sub-SELECT must produce one column per
-- target; a bare `*` positions assignment errors; database-qualified names
-- must name the current database; an aliased table's own name is an invalid
-- reference.
CREATE TABLE u117(a int, b text, c date); CREATE TABLE s117(x text, y int, z int);
INSERT INTO u117 VALUES (3, 'q', '2024-01-01'); INSERT INTO s117 VALUES ('2', 3, 4);
UPDATE u117 SET (a, b, c) = (SELECT 1, '2');
UPDATE u117 SET (a, b) = (SELECT * FROM u117);
UPDATE u117 SET b = 'x', (a, c) = (SELECT 1);
UPDATE u117 SET (a, c) = (SELECT a, b FROM u117);
UPDATE u117 SET (a, b) = (SELECT y, x FROM s117) RETURNING *;
INSERT INTO u117 SELECT * FROM s117;
INSERT INTO u117 (c)   SELECT  * FROM (SELECT 5) q;
UPDATE u117 SET b = 'z' RETURNING missing.*;
UPDATE u117 SET b = 'z' FROM s117 RETURNING nope.*;
UPDATE u117 x SET b = 'z' RETURNING u117.*;
UPDATE u117 x SET b = u117.b;
SELECT u117.a FROM u117 x;
SELECT a.b.c.d FROM u117;
SELECT u117.a.b.c.d FROM u117;
SELECT postgres.public.u117.a FROM u117;
SELECT postgres.public.u117.* FROM u117;
SELECT foo.public.u117.a FROM u117;
SELECT * FROM foo.public.u117;
SELECT a FROM postgres.public.u117;
SELECT * FROM a.b.c.d;
CREATE TABLE foo.public.t117(a int);
CREATE TABLE postgres.public.t117(a int);
SELECT * FROM t117;
DROP TABLE u117, s117, t117;
