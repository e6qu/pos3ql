-- A view prints the current names of the routines, types and relations its
-- subqueries, WITH queries and set operation members named at CREATE, after
-- they are renamed, as PostgreSQL resolves them by identity; output column
-- names keep their written spelling.
CREATE TABLE t422 (a int, b text);
CREATE TABLE u422 (a int);
CREATE FUNCTION f422(int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT $1 + 1';
CREATE FUNCTION sf422(int) RETURNS SETOF int LANGUAGE sql AS 'SELECT $1';
CREATE TYPE ty422 AS ENUM ('x', 'y');
CREATE VIEW v422_in AS SELECT a FROM t422 WHERE a IN (SELECT f422(t2.a) FROM t422 t2) AND EXISTS (SELECT 1 FROM t422 t3 WHERE t3.b::ty422 = 'x');
CREATE VIEW v422_from AS SELECT s.x, (SELECT f422(1)) AS y FROM (SELECT f422(a) AS x FROM t422) s, sf422(1) g;
CREATE VIEW v422_with AS WITH c AS (SELECT f422(a) AS z FROM u422) SELECT z FROM c UNION SELECT f422(2);
ALTER FUNCTION f422(int) RENAME TO f422_renamed;
ALTER FUNCTION sf422(int) RENAME TO sf422_renamed;
ALTER TYPE ty422 RENAME TO ty422_renamed;
ALTER TABLE u422 RENAME TO u422_renamed;
SELECT c.relname, pg_get_viewdef(c.oid) FROM pg_class c
  WHERE c.relkind = 'v' AND c.relname LIKE 'v422%' ORDER BY 1;
SELECT c.relname, pg_get_viewdef(c.oid, true) FROM pg_class c
  WHERE c.relkind = 'v' AND c.relname LIKE 'v422%' ORDER BY 1;
