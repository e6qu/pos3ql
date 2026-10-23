CREATE SCHEMA private_index_destination;
CREATE TABLE private_schema_source(key integer);
CREATE TABLE private_schema_other(key integer);
CREATE TABLE private_index_destination.competitor(key integer);
CREATE INDEX private_schema_key ON private_schema_source(key);
COMMENT ON INDEX private_schema_key IS 'key comment';
CREATE INDEX private_schema_key ON private_index_destination.competitor(key);
DO $body$
BEGIN
  BEGIN
    ALTER TABLE private_schema_source SET SCHEMA private_index_destination;
    RAISE EXCEPTION 'schema move accepted an occupied index name';
  EXCEPTION WHEN SQLSTATE '42P07' THEN
    NULL;
  END;
END
$body$;
SELECT obj_description('private_schema_key'::regclass);
DROP INDEX private_index_destination.private_schema_key;
BEGIN;
ALTER TABLE private_schema_source SET SCHEMA private_index_destination;
SELECT obj_description('private_index_destination.private_schema_key'::regclass);
SAVEPOINT moved_index;
ALTER INDEX private_index_destination.private_schema_key RENAME TO private_schema_renamed;
SELECT obj_description('private_index_destination.private_schema_renamed'::regclass);
ROLLBACK TO moved_index;
SELECT obj_description('private_index_destination.private_schema_key'::regclass);
RELEASE moved_index;
CREATE INDEX private_schema_key ON private_schema_other(key);
SELECT COALESCE(obj_description('public.private_schema_key'::regclass), 'fresh');
DO $body$
BEGIN
  BEGIN
    CREATE INDEX private_schema_key ON private_index_destination.competitor(key);
    RAISE EXCEPTION 'index creation accepted a pending schema move name';
  EXCEPTION WHEN SQLSTATE '42P07' THEN
    NULL;
  END;
END
$body$;
COMMIT;
SELECT obj_description('private_index_destination.private_schema_key'::regclass);
BEGIN;
ALTER TABLE private_index_destination.private_schema_source RENAME TO private_schema_changed;
DROP TABLE private_index_destination.private_schema_changed;
ROLLBACK;
SELECT count(*) FROM pg_indexes
WHERE schemaname = 'private_index_destination' AND tablename = 'private_schema_source';
