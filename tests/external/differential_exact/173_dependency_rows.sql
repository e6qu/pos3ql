-- pg_depend rows PostgreSQL records for partition keys, PL/pgSQL routines,
-- index and statistics columns and text search objects, and the DROPs they
-- drive: a dictionary takes the configurations that map it, and partition
-- key and inherited columns cannot be dropped alone.
CREATE SCHEMA dp;
SET search_path = dp;
CREATE TABLE t (a int NOT NULL, b text COLLATE "C", c int, d text COLLATE "default");
CREATE STATISTICS s1 ON a, c FROM t;
CREATE STATISTICS s2 ON (a + c) FROM t;
CREATE STATISTICS s3 ON a, (c * 2) FROM t;
CREATE INDEX i1 ON t (a, c);
CREATE INDEX i2 ON t ((a + 1));
CREATE INDEX i3 ON t (b) WHERE c > 0;
CREATE TABLE p (k int, v int) PARTITION BY RANGE (k);
CREATE FUNCTION f() RETURNS int LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION g() RETURNS int LANGUAGE plpgsql AS 'BEGIN RETURN 1; END';
CREATE TEXT SEARCH DICTIONARY td (TEMPLATE = simple);
CREATE TEXT SEARCH CONFIGURATION tc (PARSER = default);
CREATE TABLE u (x int DEFAULT 1::bigint::int, y numeric DEFAULT '1'::numeric(5,2));
SELECT pg_describe_object(classid, objid, objsubid) AS dependent,
       pg_describe_object(refclassid, refobjid, refobjsubid) AS referenced, deptype
FROM pg_depend
WHERE (classid, objid) IN (SELECT 'pg_class'::regclass, oid FROM pg_class WHERE relnamespace = 'dp'::regnamespace
                           UNION ALL SELECT 'pg_proc'::regclass, oid FROM pg_proc WHERE pronamespace = 'dp'::regnamespace
                           UNION ALL SELECT 'pg_statistic_ext'::regclass, oid FROM pg_statistic_ext WHERE stxnamespace = 'dp'::regnamespace
                           UNION ALL SELECT 'pg_constraint'::regclass, oid FROM pg_constraint WHERE connamespace = 'dp'::regnamespace
                           UNION ALL SELECT 'pg_ts_dict'::regclass, oid FROM pg_ts_dict WHERE dictnamespace = 'dp'::regnamespace
                           UNION ALL SELECT 'pg_ts_config'::regclass, oid FROM pg_ts_config WHERE cfgnamespace = 'dp'::regnamespace
                           UNION ALL SELECT 'pg_type'::regclass, oid FROM pg_type WHERE typnamespace = 'dp'::regnamespace)
ORDER BY 1, 2, 3;
RESET search_path;
DROP SCHEMA dp CASCADE;
CREATE TEXT SEARCH DICTIONARY td2 (TEMPLATE = snowball, LANGUAGE = english);
CREATE TEXT SEARCH CONFIGURATION tc2 (COPY = english);
ALTER TEXT SEARCH CONFIGURATION tc2 ALTER MAPPING FOR word WITH td2;
SELECT pg_describe_object(classid, objid, objsubid), pg_describe_object(refclassid, refobjid, refobjsubid), deptype
FROM pg_depend WHERE objid IN ('tc2'::regconfig, 'td2'::regdictionary) ORDER BY 1, 2;
DROP TEXT SEARCH DICTIONARY td2;
DROP TEXT SEARCH DICTIONARY td2 CASCADE;
DROP TEXT SEARCH CONFIGURATION IF EXISTS tc2;
CREATE TABLE st (a int, b int, c int);
CREATE STATISTICS sts ON a, b FROM st;
ALTER TABLE st DROP COLUMN a;
SELECT stxname FROM pg_statistic_ext WHERE stxrelid = 'st'::regclass;
CREATE TABLE pk (k int, v int) PARTITION BY RANGE (k);
ALTER TABLE pk DROP COLUMN k;
DROP TABLE st, pk;
create table par (a int, b int);
create table chi () inherits (par);
alter table chi drop column a;
create table pk (k int, v int) partition by range (k);
create table pkc partition of pk for values from (1) to (2);
alter table pkc drop column k;
alter table pk drop column k cascade;
alter table pk drop column v;
drop table pk;
drop table par cascade;
