CREATE SCHEMA extension_install;
CREATE EXTENSION pos3ql_ext VERSION '1.0' SCHEMA extension_install CASCADE;

SELECT e.extname, e.extversion, n.nspname, e.extrelocatable
FROM pg_extension AS e
JOIN pg_namespace AS n ON n.oid = e.extnamespace
WHERE e.extname IN ('pos3ql_base', 'pos3ql_ext')
ORDER BY e.extname;

SELECT name, default_version, installed_version
FROM pg_available_extensions
WHERE name IN ('pos3ql_base', 'pos3ql_ext')
ORDER BY name;

SELECT extconfig IS NOT NULL, extcondition::text
FROM pg_extension
WHERE extname = 'pos3ql_ext';

SELECT d.deptype
FROM pg_depend AS d
JOIN pg_extension AS e
  ON e.oid = d.refobjid AND d.refclassid = 'pg_extension'::regclass
JOIN pg_class AS c
  ON c.oid = d.objid AND d.classid = 'pg_class'::regclass
WHERE e.extname = 'pos3ql_ext' AND c.relname = 'extension_rows';

INSERT INTO extension_install.extension_rows VALUES (1, 'before');
ALTER EXTENSION pos3ql_ext UPDATE TO '2.0';
SELECT id, value, enabled FROM extension_install.extension_rows;

CREATE SCHEMA extension_moved;
ALTER EXTENSION pos3ql_ext SET SCHEMA extension_moved;
SELECT n.nspname, e.extversion
FROM pg_extension AS e
JOIN pg_namespace AS n ON n.oid = e.extnamespace
WHERE e.extname = 'pos3ql_ext';
SELECT extension_moved.extension_identity('moved') AS identity;

CREATE TABLE extension_member(id integer);
ALTER EXTENSION pos3ql_ext ADD TABLE extension_member;
SELECT deptype
FROM pg_depend
WHERE classid = 'pg_class'::regclass
  AND objid = 'extension_member'::regclass
  AND refclassid = 'pg_extension'::regclass
  AND refobjid = (SELECT oid FROM pg_extension WHERE extname = 'pos3ql_ext');
ALTER EXTENSION pos3ql_ext DROP TABLE extension_member;

CREATE TABLE extension_auto_source(id integer);
CREATE INDEX extension_auto_index ON extension_auto_source(id);
ALTER INDEX extension_auto_index DEPENDS ON EXTENSION pos3ql_ext;
CREATE MATERIALIZED VIEW extension_auto_matview AS SELECT 10 AS value;
ALTER MATERIALIZED VIEW extension_auto_matview DEPENDS ON EXTENSION pos3ql_ext;
CREATE MATERIALIZED VIEW extension_survivor AS SELECT 20 AS value;
ALTER MATERIALIZED VIEW extension_survivor DEPENDS ON EXTENSION pos3ql_ext;
ALTER MATERIALIZED VIEW extension_survivor NO DEPENDS ON EXTENSION pos3ql_ext;
CREATE FUNCTION extension_auto_function() RETURNS integer
LANGUAGE SQL AS $$ SELECT 30 $$;
ALTER FUNCTION extension_auto_function DEPENDS ON EXTENSION pos3ql_ext;
CREATE FUNCTION extension_auto_state(state bigint, value integer) RETURNS bigint
LANGUAGE SQL AS $$ SELECT coalesce(state, 0) + value $$;
ALTER FUNCTION extension_auto_state(bigint, integer) DEPENDS ON EXTENSION pos3ql_ext;
CREATE AGGREGATE extension_auto_sum(integer) (
  SFUNC = extension_auto_state,
  STYPE = bigint,
  INITCOND = '0'
);
ALTER ROUTINE extension_auto_sum(integer) DEPENDS ON EXTENSION pos3ql_ext;

SELECT count(*)
FROM pg_depend AS d
JOIN pg_extension AS e
  ON e.oid = d.refobjid AND d.refclassid = 'pg_extension'::regclass
WHERE e.extname = 'pos3ql_ext' AND d.deptype = 'x';

CREATE FUNCTION extension_ambiguous() RETURNS integer
LANGUAGE SQL AS $$ SELECT 1 $$;
CREATE FUNCTION extension_ambiguous(integer) RETURNS integer
LANGUAGE SQL AS $$ SELECT $1 $$;
ALTER FUNCTION extension_ambiguous DEPENDS ON EXTENSION pos3ql_ext;
ALTER AGGREGATE extension_auto_sum(integer) DEPENDS ON EXTENSION pos3ql_ext;
ALTER INDEX IF EXISTS extension_auto_index DEPENDS ON EXTENSION pos3ql_ext;
ALTER MATERIALIZED VIEW IF EXISTS extension_auto_matview DEPENDS ON EXTENSION pos3ql_ext;

DROP EXTENSION pos3ql_base;
DROP EXTENSION pos3ql_ext;
SELECT count(*) FROM pg_extension WHERE extname = 'pos3ql_base';
SELECT count(*) FROM pg_class
WHERE relname IN ('extension_auto_index', 'extension_auto_matview');
SELECT count(*) FROM pg_proc
WHERE proname IN ('extension_auto_function', 'extension_auto_state', 'extension_auto_sum');
SELECT value FROM extension_survivor;
SELECT count(*) FROM extension_member;

DROP EXTENSION pos3ql_base;
DROP MATERIALIZED VIEW extension_survivor;
DROP TABLE extension_member, extension_auto_source;
DROP FUNCTION extension_ambiguous();
DROP FUNCTION extension_ambiguous(integer);
DROP SCHEMA extension_install CASCADE;
DROP SCHEMA extension_moved CASCADE;
