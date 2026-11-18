-- A COLLATE on a type without collation, or naming no collation, fails where
-- it is written, with PostgreSQL's cursor: in a column or attribute
-- definition, an ALTER COLUMN or ATTRIBUTE TYPE, a domain, and on a domain
-- value (named by the domain, not its base type). Index keys carry no cursor.
CREATE TABLE ct861 (a text, n integer);
CREATE DOMAIN dint861 AS integer;
CREATE DOMAIN dtxt861 AS text;
CREATE TABLE cd861 (x dint861, y dtxt861);
CREATE INDEX ON ct861 (a COLLATE "nosuch");
CREATE INDEX ON ct861 (n COLLATE "C");
CREATE INDEX ON ct861 ((n + 1) COLLATE "C");
CREATE INDEX ON ct861 (a COLLATE "C", n COLLATE "nosuch");
ALTER TABLE ct861 ALTER COLUMN n TYPE integer COLLATE "C";
ALTER TABLE ct861 ALTER COLUMN a TYPE text COLLATE "nosuch";
ALTER TABLE ct861 ADD COLUMN z integer COLLATE "C";
CREATE TYPE cr861 AS (a text COLLATE "nosuch");
CREATE TYPE cr861 AS (a integer COLLATE "C");
CREATE TYPE cr861 AS (a text COLLATE "C");
ALTER TYPE cr861 ADD ATTRIBUTE b integer COLLATE "C";
ALTER TYPE cr861 ADD ATTRIBUTE b text COLLATE "nosuch";
ALTER TYPE cr861 ALTER ATTRIBUTE a TYPE integer COLLATE "C";
SELECT x COLLATE "C" FROM cd861;
SELECT y COLLATE "C" FROM cd861;
SELECT y COLLATE "nosuch" FROM cd861;
SELECT x COLLATE "nosuch" FROM cd861;
SELECT 1::dint861 COLLATE "C";
SELECT 'a'::dtxt861 COLLATE "C";
CREATE TABLE ctt861 (a text, CONSTRAINT x EXCLUDE (a COLLATE "nosuch" WITH =));
CREATE TABLE ctu861 (a integer, UNIQUE (a COLLATE "C"));
DROP COLLATION nosuch861;
DROP COLLATION public.nosuch861;
DROP COLLATION nosuch861x.c;
DROP COLLATION IF EXISTS nosuch861;
COMMENT ON COLLATION nosuch861 IS 'x';
CREATE COLLATION c861 FROM nosuch861;
ALTER COLLATION nosuch861 REFRESH VERSION;
SELECT 'nosuch861'::regcollation;
