CREATE TABLE metadata_replaced_table(value integer);
COMMENT ON TABLE metadata_replaced_table IS 'original';
BEGIN;
COMMENT ON TABLE metadata_replaced_table IS 'retiring';
DROP TABLE metadata_replaced_table;
CREATE TABLE metadata_replaced_table(value integer);
SELECT COALESCE(obj_description('metadata_replaced_table'::regclass, 'pg_class'), 'fresh');
COMMENT ON TABLE metadata_replaced_table IS 'replacement';
COMMIT;
SELECT obj_description('metadata_replaced_table'::regclass, 'pg_class');
DROP TABLE metadata_replaced_table;

CREATE MATERIALIZED VIEW metadata_replaced_materialized AS SELECT 1 AS value;
COMMENT ON MATERIALIZED VIEW metadata_replaced_materialized IS 'original';
BEGIN;
COMMENT ON MATERIALIZED VIEW metadata_replaced_materialized IS 'retiring';
DROP MATERIALIZED VIEW metadata_replaced_materialized;
CREATE MATERIALIZED VIEW metadata_replaced_materialized AS SELECT 2 AS value;
SELECT COALESCE(obj_description('metadata_replaced_materialized'::regclass, 'pg_class'), 'fresh');
COMMENT ON MATERIALIZED VIEW metadata_replaced_materialized IS 'replacement';
COMMIT;
SELECT value FROM metadata_replaced_materialized;
SELECT obj_description('metadata_replaced_materialized'::regclass, 'pg_class');
DROP MATERIALIZED VIEW metadata_replaced_materialized;

CREATE TYPE metadata_shared_name AS ENUM ('value');
COMMENT ON TYPE metadata_shared_name IS 'type comment';
DO $body$
BEGIN
  BEGIN
    EXECUTE 'CREATE SEQUENCE metadata_shared_name';
    RAISE EXCEPTION 'sequence creation accepted a conflicting type name';
EXCEPTION WHEN SQLSTATE '42710' THEN
    NULL;
  END;
END
$body$;
SELECT obj_description('metadata_shared_name'::regtype, 'pg_type');
DROP TYPE metadata_shared_name;
