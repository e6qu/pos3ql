-- Sequence definition and restart semantics, matched against PostgreSQL 18.4.
DROP SEQUENCE IF EXISTS staged_sequence_diff;

CREATE SEQUENCE staged_sequence_diff START WITH 10 INCREMENT BY 2;
SELECT nextval('staged_sequence_diff');

BEGIN;
ALTER SEQUENCE staged_sequence_diff INCREMENT BY 7 RESTART WITH 40;
SELECT increment_by, last_value FROM pg_sequences WHERE sequencename = 'staged_sequence_diff';
SAVEPOINT preserved_definition;
SELECT nextval('staged_sequence_diff');
ALTER SEQUENCE staged_sequence_diff INCREMENT BY 11 RESTART WITH 80;
ROLLBACK TO SAVEPOINT preserved_definition;
SELECT nextval('staged_sequence_diff');
COMMIT;

SELECT increment_by, last_value FROM pg_sequences WHERE sequencename = 'staged_sequence_diff';
SELECT nextval('staged_sequence_diff');
DROP SEQUENCE staged_sequence_diff;

CREATE SEQUENCE cached_sequence_diff MINVALUE 1 MAXVALUE 9 CACHE 5;
SELECT nextval('cached_sequence_diff');
SELECT last_value, log_cnt, is_called FROM cached_sequence_diff;
SELECT nextval('cached_sequence_diff');
SELECT last_value, log_cnt, is_called FROM cached_sequence_diff;
ALTER SEQUENCE cached_sequence_diff INCREMENT BY 2;
SELECT last_value, log_cnt, is_called FROM cached_sequence_diff;
SELECT nextval('cached_sequence_diff');
SELECT last_value, log_cnt, is_called FROM cached_sequence_diff;
DROP SEQUENCE cached_sequence_diff;

CREATE SEQUENCE cycling_cache_diff MINVALUE 1 MAXVALUE 3 CACHE 5 CYCLE;
SELECT nextval('cycling_cache_diff') FROM generate_series(1, 5);
SELECT last_value, log_cnt, is_called FROM cycling_cache_diff;
DROP SEQUENCE cycling_cache_diff;

-- A sequence rename preserves the catalog identity, current value, comment,
-- transaction visibility, and WAL/checkpoint recovery identity.
CREATE SCHEMA sequence_rename_schema_diff;
CREATE SEQUENCE sequence_rename_source_diff START WITH 40 INCREMENT BY 3;
CREATE TABLE sequence_rename_default_diff (
  id bigint DEFAULT nextval('sequence_rename_source_diff')
);
CREATE VIEW sequence_rename_view_diff AS
  WITH value AS MATERIALIZED (SELECT nextval('sequence_rename_source_diff') AS id)
  SELECT id FROM value;
COMMENT ON SEQUENCE sequence_rename_source_diff IS 'renamed sequence';
SELECT nextval('sequence_rename_source_diff');
ALTER SEQUENCE sequence_rename_source_diff RENAME TO sequence_rename_target_diff;
SELECT nextval('sequence_rename_target_diff');
INSERT INTO sequence_rename_default_diff DEFAULT VALUES RETURNING id;
SELECT id FROM sequence_rename_view_diff;
SELECT nextval('sequence_rename_target_diff');
SELECT relname FROM pg_class WHERE relname = 'sequence_rename_target_diff';
SELECT obj_description('sequence_rename_target_diff'::regclass);
SELECT nextval('sequence_rename_source_diff');
BEGIN;
ALTER SEQUENCE sequence_rename_target_diff RENAME TO sequence_rename_rolled_back_diff;
SELECT nextval('sequence_rename_rolled_back_diff');
ROLLBACK;
SELECT nextval('sequence_rename_target_diff');
BEGIN;
ALTER SEQUENCE sequence_rename_target_diff RESTART WITH 100;
SELECT nextval('sequence_rename_target_diff');
ROLLBACK;
SELECT nextval('sequence_rename_target_diff');
ALTER SEQUENCE sequence_rename_target_diff SET SCHEMA sequence_rename_schema_diff;
ALTER SEQUENCE sequence_rename_schema_diff.sequence_rename_target_diff
  RENAME TO sequence_rename_durable_diff;
SELECT nextval('sequence_rename_schema_diff.sequence_rename_durable_diff');
INSERT INTO sequence_rename_default_diff DEFAULT VALUES RETURNING id;
DROP VIEW sequence_rename_view_diff;
DROP TABLE sequence_rename_default_diff;
DROP SEQUENCE sequence_rename_schema_diff.sequence_rename_durable_diff;
DROP SCHEMA sequence_rename_schema_diff;
