-- Rule definitions print their actions as PostgreSQL deparses them, a rule
-- condition rejects an aggregate of its own rows even inside a subquery, and
-- CREATE OR REPLACE RULE "_RETURN" keeps a view column's type modifier when
-- the new query's column has none. Unknown-type literals passed to
-- overloaded builtins, in VALUES lists and in set operations convert while
-- the statement is analysed, positioned at the literal.
CREATE TABLE t360 (a int, b text);
CREATE TABLE log360 (a int, b text);
CREATE RULE ins360 AS ON INSERT TO t360 WHERE new.a > 0
  DO ALSO INSERT INTO log360 VALUES (new.a, new.b);
CREATE RULE upd360 AS ON UPDATE TO t360
  DO INSTEAD (UPDATE log360 SET b = new.b WHERE a = old.a; DELETE FROM log360 WHERE a < 0);
CREATE RULE del360 AS ON DELETE TO t360 DO ALSO NOTIFY chan360;
CREATE RULE def360 AS ON INSERT TO log360 WHERE new.a = 99
  DO INSTEAD INSERT INTO t360 (b) VALUES (DEFAULT);
SELECT rulename, pg_get_ruledef(oid) FROM pg_rewrite
  WHERE rulename LIKE '%360' ORDER BY rulename;
SELECT rulename, pg_get_ruledef(oid, true) FROM pg_rewrite
  WHERE rulename LIKE '%360' ORDER BY rulename;
SELECT rulename, definition FROM pg_rules WHERE tablename = 't360' ORDER BY rulename;
CREATE RULE agg360 AS ON INSERT TO t360 WHERE (SELECT sum(new.a)) > 0
  DO ALSO NOTIFY chan360;
CREATE RULE agg360 AS ON INSERT TO t360 WHERE (SELECT max(log360.a) FROM log360) > new.a
  DO ALSO NOTIFY chan360;
CREATE RULE aggact360 AS ON INSERT TO t360 DO ALSO SELECT (SELECT sum(new.a));
DROP RULE agg360 ON t360;
DROP RULE aggact360 ON t360;
SELECT (SELECT sum(t360.a)) FROM t360;
INSERT INTO t360 VALUES (1, 'x'), (2, 'y');
SELECT (SELECT sum(t360.a)) FROM t360;
SELECT * FROM t360 WHERE (SELECT sum(t360.a)) > 0;

CREATE TABLE src360 (n numeric(5,2), s varchar(3));
INSERT INTO src360 VALUES (1.234, 'ab');
CREATE VIEW v360 AS SELECT n, s FROM src360;
CREATE OR REPLACE RULE "_RETURN" AS ON SELECT TO v360
  DO INSTEAD SELECT n::numeric AS n, s::text::varchar AS s FROM src360;
SELECT attname, format_type(atttypid, atttypmod) FROM pg_attribute
  WHERE attrelid = 'v360'::regclass AND attnum > 0 ORDER BY attnum;
SELECT * FROM v360;
CREATE OR REPLACE RULE "_RETURN" AS ON SELECT TO v360
  DO INSTEAD SELECT n::numeric(6,1) AS n, s FROM src360;
CREATE OR REPLACE RULE "_RETURN" AS ON SELECT TO v360
  DO INSTEAD SELECT 1.5::numeric AS n, 'abcdef'::varchar AS s;
SELECT * FROM v360;

SELECT substr('abcdef', '2'), substr('abcdef', '2', '3'), round('1.5'),
  pg_typeof(round('1.5')), sqrt('4'), power('2', '3');
SELECT substr('abc', 'x');
SELECT substr('abc', 'x') FROM t360;
SELECT abs('x');
SELECT int4pl(1, 'p');
VALUES (1), ('p');
VALUES (1, 'a'), ('x', 'b');
SELECT * FROM (VALUES (1), ('p')) v;
SELECT 1 UNION SELECT 'p';
SELECT 'p' UNION SELECT 1;
SELECT 'a' UNION ALL SELECT 'b' UNION SELECT 1;
SELECT NULL UNION SELECT 'x' UNION SELECT 1;
SELECT NULL UNION SELECT NULL UNION SELECT 1 ORDER BY 1;
VALUES (NULL), (NULL), (1);

SELECT DISTINCT ON (5) 1;
SELECT DISTINCT ON (x) 1;
SELECT DISTINCT ON (1) 1, 2 ORDER BY 2;
SELECT 1 ORDER BY 3;
SELECT DISTINCT ON (5) a, b FROM t360;

CREATE INDEX ON t360 ((ax + 1));
CREATE INDEX ON t360 (a) WHERE ax > 0;
COPY t360 FROM stdin WHERE ax > 0;
\.
COPY t360 FROM stdin WHERE t360.bb > 'a';
\.
DROP VIEW v360;
DROP TABLE t360, log360, src360;
