-- A view's column types, modifiers and collations are fixed when it is
-- created; later uses, CREATE OR REPLACE VIEW and ALTER COLUMN TYPE see them.
CREATE SCHEMA vct;
SET search_path = vct;
CREATE FUNCTION sfunc(int, int) RETURNS int LANGUAGE sql AS 'SELECT coalesce($1, 0) + $2';
CREATE AGGREGATE agg(int) (SFUNC = sfunc, STYPE = int);
CREATE VIEW av AS SELECT agg(x) FROM generate_series(1, 3) x;
SELECT * FROM av;
CREATE VIEW av2 AS SELECT agg(g) FROM generate_series(1, 3) AS s(g);
SELECT * FROM av2;
SELECT attname, atttypid::regtype, atttypmod, attcollation FROM pg_attribute
 WHERE attrelid = 'av'::regclass AND attnum > 0;
CREATE DOMAIN d AS varchar(5);
CREATE TYPE mood AS ENUM ('sad', 'ok');
CREATE TYPE pair AS (a int, b text);
CREATE TABLE t (a varchar(3), b text, c numeric(5,2), d d, m mood, p pair);
CREATE VIEW agg_t AS SELECT agg(length(a)) AS total FROM t;
CREATE VIEW v AS SELECT a, b COLLATE "C" AS bc, c, d, m, p, a::char(2) AS ch FROM t;
SELECT attname, atttypid::regtype, atttypmod, attcollation::regcollation
  FROM pg_attribute WHERE attrelid = 'v'::regclass AND attnum > 0 ORDER BY attnum;
SELECT column_name, data_type, character_maximum_length, collation_name
  FROM information_schema.columns WHERE table_name = 'v' ORDER BY ordinal_position;
\d v
CREATE OR REPLACE VIEW v AS SELECT a::varchar(4) AS a, b COLLATE "C" AS bc, c, d, m, p, a::char(2) AS ch FROM t;
CREATE OR REPLACE VIEW v AS SELECT a, b AS bc, c, d, m, p, a::char(2) AS ch FROM t;
CREATE OR REPLACE VIEW v AS SELECT a, b COLLATE "C" AS bc, c::numeric AS c, d, m, p, a::char(2) AS ch FROM t;
CREATE OR REPLACE VIEW v AS SELECT a, b COLLATE "C" AS bc, c, d::varchar(5) AS d, m, p, a::char(2) AS ch FROM t;
CREATE OR REPLACE VIEW v AS SELECT a AS z, b COLLATE "C" AS bc, c, d, m, p, a::char(2) AS ch FROM t;
CREATE OR REPLACE VIEW v AS SELECT a, b COLLATE "C" AS bc, c, d, m, p, a::char(2) AS ch, 1 AS extra FROM t;
SELECT attname, atttypid::regtype FROM pg_attribute
 WHERE attrelid = 'v'::regclass AND attnum > 0 ORDER BY attnum;
CREATE VIEW vi AS SELECT x || y FROM (SELECT 'a' COLLATE "C" AS x, 'b' COLLATE "POSIX" AS y) s;
CREATE VIEW vi AS SELECT a || bc AS z FROM v;
SELECT attname, attcollation::regcollation FROM pg_attribute WHERE attrelid = 'vi'::regclass AND attnum > 0;
INSERT INTO t VALUES ('ab', 'Q', 1.5, 'dd', 'ok', ROW(1, 'x'));
SELECT bc < 'a' FROM v;
SELECT bc < 'a' COLLATE "POSIX" FROM v;
SELECT * FROM v;
SELECT * FROM agg_t;
-- PostgreSQL refuses to change the type of a column a view, policy or
-- SQL-standard routine body reads.
ALTER TABLE t ALTER COLUMN a TYPE varchar(10);
ALTER TABLE t ALTER COLUMN c TYPE numeric(6,2);
ALTER TYPE pair ADD ATTRIBUTE c int;
SELECT p FROM v;
ALTER TYPE pair RENAME TO pair2;
SELECT attname, atttypid::regtype FROM pg_attribute WHERE attrelid = 'v'::regclass AND attname = 'p';
CREATE TABLE u (a int, b int, c int);
CREATE POLICY up ON u USING (b > 0);
ALTER TABLE u ALTER COLUMN b TYPE bigint;
CREATE FUNCTION uf() RETURNS bigint LANGUAGE sql BEGIN ATOMIC SELECT c FROM u; END;
ALTER TABLE u ALTER COLUMN c TYPE bigint;
CREATE VIEW u1 AS SELECT a FROM u;
CREATE VIEW u2 AS SELECT a FROM u;
ALTER TABLE u ALTER COLUMN a TYPE bigint;
DROP VIEW u1, u2;
ALTER TABLE u ALTER COLUMN a TYPE bigint;
RESET search_path;
DROP SCHEMA vct CASCADE;
-- Trigger WHEN conditions and rules record the columns NEW and OLD read.
CREATE SCHEMA vct2;
SET search_path = vct2;
CREATE TABLE t (a int, b int, c int, d int, e int);
CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
CREATE TRIGGER tr BEFORE UPDATE ON t FOR EACH ROW WHEN (NEW.b > 0 AND OLD.a < 1) EXECUTE FUNCTION tf();
ALTER TABLE t ALTER COLUMN b TYPE bigint;
CREATE TABLE r (x int, y int);
CREATE RULE ru AS ON INSERT TO r DO ALSO DELETE FROM t WHERE e = NEW.x;
CREATE RULE ru2 AS ON UPDATE TO r WHERE OLD.y > 0 DO INSTEAD DELETE FROM t WHERE c = OLD.x;
ALTER TABLE t ALTER COLUMN e TYPE bigint;
ALTER TABLE r ALTER COLUMN x TYPE bigint;
SELECT pg_describe_object(classid, objid, objsubid) AS dependent,
       pg_describe_object(refclassid, refobjid, refobjsubid) AS referenced, deptype
  FROM pg_depend
 WHERE classid IN ('pg_rewrite'::regclass, 'pg_trigger'::regclass)
   AND objid >= 16384 AND refobjid >= 16384
   AND objid IN (SELECT oid FROM pg_rewrite
                  WHERE ev_class IN (SELECT oid FROM pg_class WHERE relnamespace = 'vct2'::regnamespace)
                 UNION ALL
                 SELECT oid FROM pg_trigger
                  WHERE tgrelid IN (SELECT oid FROM pg_class WHERE relnamespace = 'vct2'::regnamespace))
 ORDER BY 1, 2;
RESET search_path;
DROP SCHEMA vct2 CASCADE;
