-- View metadata must derive from the same transactional identities that drive
-- execution, dependency checks, WAL recovery, and client-tool catalog reads.
DROP VIEW IF EXISTS view_catalog_joined;
DROP VIEW IF EXISTS view_catalog_predicate;
DROP VIEW IF EXISTS view_catalog_simple;
DROP TABLE IF EXISTS view_catalog_source;

CREATE TABLE view_catalog_source (id integer, value text);
CREATE VIEW view_catalog_simple AS SELECT id, value FROM view_catalog_source;
CREATE VIEW view_catalog_predicate AS
  SELECT value || value AS doubled FROM view_catalog_source WHERE id > 0;
CREATE VIEW view_catalog_joined AS
  SELECT source.id
  FROM view_catalog_source source
  JOIN view_catalog_simple exposed ON exposed.id = source.id;

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
DROP TABLE view_catalog_source;
