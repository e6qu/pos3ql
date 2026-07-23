-- Schemas: CREATE/DROP SCHEMA, schema-qualified DDL and DML, search_path
-- resolution, current_schema/current_schemas, dependency reports, cascades
-- across schemas, views bound to their creation path, and ALTER TABLE SET
-- SCHEMA. Session state is reset at the end so later corpora are unaffected.
SET search_path = "$user", public;

-- Creation, duplicates, reserved names.
CREATE SCHEMA s1;
CREATE SCHEMA s1;
CREATE SCHEMA IF NOT EXISTS s1;
CREATE SCHEMA pg_reserved_test;
CREATE SCHEMA information_schema;
CREATE SCHEMA public;

-- Qualified DDL and DML without touching search_path.
CREATE TABLE s1.t(a int, b text);
INSERT INTO s1.t VALUES (1, 'one'), (2, 'two');
SELECT * FROM s1.t ORDER BY a;
UPDATE s1.t SET b = 'ONE' WHERE a = 1;
DELETE FROM s1.t WHERE a = 2;
SELECT * FROM s1.t;
SELECT count(*) FROM t;
CREATE TABLE nosuch.t(a int);
SELECT * FROM nosuch.t;
DROP TABLE nosuch.t;
INSERT INTO nosuch.t VALUES (1);

-- Same table name in two schemas; the path picks the first.
CREATE TABLE public.t(a int, b text);
INSERT INTO public.t VALUES (10, 'pub');
SELECT * FROM t;
SET search_path = s1, public;
SHOW search_path;
SELECT * FROM t;
SELECT current_schema(), current_schemas(false), current_schemas(true);

-- Creation lands in the first existing path schema.
CREATE TABLE t2(x int);
SELECT schemaname, tablename FROM pg_tables WHERE tablename = 't2';
SET search_path = ghost, s1;
SELECT current_schema();
CREATE TABLE t3(y int);
SELECT schemaname, tablename FROM pg_tables WHERE tablename = 't3';
SET search_path = ghost;
SELECT current_schema(), current_schemas(false);
CREATE TABLE t4(z int);
SELECT * FROM t;
SET search_path = s1, public;

-- An unqualified DROP TABLE resolves through the path too.
DROP TABLE t;
SELECT * FROM public.t;
SELECT * FROM s1.t;

-- Serial sequences live per table, in any schema.
CREATE TABLE s1.ser(id serial, v text);
INSERT INTO s1.ser(v) VALUES ('x'), ('y') RETURNING id;
SELECT * FROM s1.ser ORDER BY id;

-- Indexes follow their table's schema; DROP INDEX resolves through the path.
CREATE UNIQUE INDEX t2_x_idx ON t2(x);
INSERT INTO t2 VALUES (5), (5);
DROP INDEX t2_x_idx;
INSERT INTO t2 VALUES (5), (5);
SELECT count(*) FROM t2;

-- RESTRICT refuses a non-empty schema with the dependency report.
DROP SCHEMA s1;
-- CASCADE drops the contents.
CREATE SCHEMA s2;
CREATE TABLE s2.only_one(a int);
DROP SCHEMA s2 CASCADE;
SELECT * FROM s2.only_one;
DROP SCHEMA nosuch;
DROP SCHEMA IF EXISTS nosuch;
DROP SCHEMA pg_catalog;

-- A foreign key referencing another schema; dropping the parent's schema
-- severs the constraint but keeps the child table.
CREATE SCHEMA sa;
CREATE SCHEMA sb;
CREATE TABLE sa.p(id int PRIMARY KEY);
CREATE TABLE sb.c(id int REFERENCES sa.p);
INSERT INTO sa.p VALUES (1);
INSERT INTO sb.c VALUES (1);
INSERT INTO sb.c VALUES (99);
DROP SCHEMA sa CASCADE;
SELECT count(*) FROM sb.c;
INSERT INTO sb.c VALUES (99);
SELECT count(*) FROM sb.c;

-- Views: created in a schema, resolved under their creator's search path.
SET search_path = sb, public;
CREATE TABLE sb.vt(n int);
INSERT INTO sb.vt VALUES (7);
CREATE VIEW sb.v AS SELECT n FROM vt;
SELECT * FROM v;
SET search_path = public;
SELECT * FROM sb.v;
CREATE TABLE public.vt(n int);
INSERT INTO public.vt VALUES (100);
SELECT * FROM sb.v;
SELECT * FROM vt;
DROP TABLE public.vt;

-- CREATE SCHEMA with embedded elements, and the mismatched-qualifier error.
CREATE SCHEMA sc CREATE TABLE inner_t(a int) CREATE VIEW inner_v AS SELECT 5 AS five;
SELECT * FROM sc.inner_t;
SELECT * FROM sc.inner_v;
CREATE SCHEMA sd CREATE TABLE se.wrong(a int);
CREATE SCHEMA authorization postgres;
DROP SCHEMA postgres CASCADE;
CREATE SCHEMA au2 AUTHORIZATION nosuchrole;

-- ALTER TABLE SET SCHEMA moves the table; inbound references follow.
CREATE TABLE public.mv(a int);
INSERT INTO public.mv VALUES (42);
ALTER TABLE public.mv SET SCHEMA sc;
SELECT * FROM sc.mv;
SELECT count(*) FROM public.mv;
CREATE TABLE sc.taken(a int);
CREATE TABLE public.taken(a int);
ALTER TABLE public.taken SET SCHEMA sc;
ALTER TABLE public.taken SET SCHEMA ghost;

-- Multi-name DROP TABLE across schemas.
CREATE TABLE public.d1(a int);
CREATE TABLE sc.d2(a int);
DROP TABLE public.d1, sc.d2;
DROP TABLE public.d1, sc.d2;
DROP TABLE IF EXISTS public.d1, sc.d2;

-- TRUNCATE with qualified names.
CREATE TABLE sc.tr(a int);
INSERT INTO sc.tr VALUES (1), (2);
TRUNCATE sc.tr;
SELECT count(*) FROM sc.tr;

-- Transactional schema DDL: rollback undoes CREATE SCHEMA and its contents.
BEGIN;
CREATE SCHEMA txs;
CREATE TABLE txs.tt(a int);
INSERT INTO txs.tt VALUES (1);
SELECT * FROM txs.tt;
ROLLBACK;
SELECT * FROM txs.tt;
CREATE SCHEMA txs;
BEGIN;
DROP SCHEMA txs;
ROLLBACK;
CREATE TABLE txs.still(a int);
SELECT schemaname, tablename FROM pg_tables WHERE tablename = 'still';

-- Catalog views reflect schemas.
SELECT nspname FROM pg_namespace WHERE nspname IN ('public', 's1', 'sb', 'sc', 'txs') ORDER BY nspname;
SELECT schemaname, tablename FROM pg_tables WHERE schemaname IN ('sb', 'sc', 'txs') ORDER BY schemaname, tablename;
SELECT table_schema, table_name FROM information_schema.tables WHERE table_schema IN ('sb', 'sc') ORDER BY table_schema, table_name;
SELECT schema_name FROM information_schema.schemata WHERE schema_name IN ('public', 'sb', 'sc') ORDER BY schema_name;

-- Search-path canonicalization for SHOW.
SET search_path = PUBLIC;
SHOW search_path;
SET search_path = "Weird Path", public;
SHOW search_path;
SET search_path = 'sb, public';
SHOW search_path;
SELECT current_schemas(false);
SET search_path = pg_catalog, public;
SELECT current_schema();
SELECT current_schemas(true);
CREATE TABLE cat_first(a int);
SELECT schemaname FROM pg_tables WHERE tablename = 'cat_first';

-- Cleanup, restoring the default path for later corpora.
SET search_path = "$user", public;
DROP SCHEMA s1, sb, sc, sd, txs CASCADE;
DROP SCHEMA s1, sb, sc, txs CASCADE;
DROP SCHEMA IF EXISTS sd CASCADE;
DROP TABLE public.t, public.taken;
DROP SCHEMA pg_reserved_test;
-- Only this corpus's own objects: the harness may run other suites against
-- either engine first, so a whole-schema count would not be hermetic.
SELECT count(*) FROM pg_tables WHERE schemaname = 'public' AND tablename IN ('t', 'taken', 'd1', 'mv', 'vt', 't2', 'cat_first');
