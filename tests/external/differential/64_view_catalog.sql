-- View metadata must derive from the same transactional identities that drive
-- execution, dependency checks, WAL recovery, and client-tool catalog reads.
DROP VIEW IF EXISTS view_catalog_joined;
DROP VIEW IF EXISTS view_catalog_predicate;
DROP VIEW IF EXISTS view_catalog_simple;
DROP VIEW IF EXISTS view_check_option;
DROP VIEW IF EXISTS view_security_barrier;
DROP VIEW IF EXISTS view_catalog_alias;
DROP TABLE IF EXISTS view_catalog_source;
DROP TABLE IF EXISTS view_check_option_source;

CREATE TABLE view_catalog_source (id integer, value text);
CREATE VIEW view_catalog_simple AS SELECT id, value FROM view_catalog_source;
CREATE VIEW view_catalog_alias (published_id, published_value) AS
  SELECT id, value FROM view_catalog_source;
CREATE VIEW view_security_barrier WITH (security_barrier = true) AS
  SELECT id, value FROM view_catalog_source;
SELECT reloptions FROM pg_class WHERE relname = 'view_security_barrier';
ALTER VIEW view_security_barrier
  SET (security_invoker = true, security_barrier = false);
SELECT reloptions FROM pg_class WHERE relname = 'view_security_barrier';
ALTER VIEW view_security_barrier RESET (security_invoker, security_barrier);
SELECT reloptions FROM pg_class WHERE relname = 'view_security_barrier';
INSERT INTO view_catalog_alias (published_id, published_value) VALUES (1, 'one');
UPDATE view_catalog_alias SET published_value = 'updated' WHERE published_id = 1;
ALTER VIEW view_catalog_alias RENAME COLUMN published_value TO current_value;
ALTER VIEW view_catalog_alias ALTER COLUMN current_value SET DEFAULT 'view default';
INSERT INTO view_catalog_alias (published_id) VALUES (2);
INSERT INTO view_catalog_alias (published_id, current_value) VALUES (3, DEFAULT);
SELECT published_id, current_value FROM view_catalog_alias;
SELECT attname FROM pg_attribute
 WHERE attrelid = 'view_catalog_alias'::regclass AND attnum > 0
 ORDER BY attnum;
SELECT atthasdef FROM pg_attribute
 WHERE attrelid = 'view_catalog_alias'::regclass AND attname = 'current_value';
ALTER VIEW view_catalog_alias ALTER COLUMN current_value DROP DEFAULT;
CREATE VIEW view_catalog_predicate AS
  SELECT value || value AS doubled FROM view_catalog_source WHERE id > 0;
CREATE VIEW view_catalog_joined AS
  SELECT source.id
  FROM view_catalog_source source
  JOIN view_catalog_simple exposed ON exposed.id = source.id;
CREATE TABLE view_check_option_source (value integer);
CREATE VIEW view_check_option WITH (check_option = local) AS
  SELECT value FROM view_check_option_source WHERE value > 0;
INSERT INTO view_check_option VALUES (1);
INSERT INTO view_check_option VALUES (-1);
UPDATE view_check_option SET value = -1;

SELECT check_option
  FROM information_schema.views
 WHERE table_name = 'view_check_option';
SELECT value FROM view_check_option_source;
ALTER VIEW view_check_option SET (check_option = cascaded);
SELECT check_option
  FROM information_schema.views
 WHERE table_name = 'view_check_option';

SELECT table_name, check_option, is_updatable, is_insertable_into,
       is_trigger_updatable, is_trigger_deletable, is_trigger_insertable_into
  FROM information_schema.views
 WHERE table_name IN ('view_catalog_simple', 'view_catalog_predicate', 'view_catalog_joined')
 ORDER BY table_name;

SELECT view_name, table_name
  FROM information_schema.view_table_usage
 WHERE view_name IN ('view_catalog_simple', 'view_catalog_joined')
 ORDER BY view_name, table_name;

SELECT view_name, table_name, column_name
  FROM information_schema.view_column_usage
 WHERE view_name = 'view_catalog_simple'
 ORDER BY column_name;

SELECT column_name
  FROM information_schema.view_column_usage
 WHERE view_name = 'view_catalog_predicate'
 ORDER BY column_name;

SELECT table_name, column_name
  FROM information_schema.view_column_usage
 WHERE view_name = 'view_catalog_joined'
 ORDER BY table_name, column_name;

SELECT rulename, ev_type, ev_enabled, is_instead
  FROM pg_rewrite
 WHERE ev_class = 'view_catalog_simple'::regclass;

SELECT d.deptype, c.relname
  FROM pg_depend d
  JOIN pg_class c ON c.oid = d.refobjid
 WHERE d.classid = 'pg_rewrite'::regclass
   AND d.objid = (SELECT oid FROM pg_rewrite
                  WHERE ev_class = 'view_catalog_simple'::regclass)
 ORDER BY d.deptype, c.relname;

DROP VIEW view_catalog_joined;
DROP VIEW view_catalog_predicate;
DROP VIEW view_catalog_simple;
DROP VIEW view_catalog_alias;
DROP VIEW view_check_option;
DROP VIEW view_security_barrier;
DROP TABLE view_catalog_source;
DROP TABLE view_check_option_source;
