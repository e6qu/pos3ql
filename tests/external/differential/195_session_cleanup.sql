-- Temporary namespaces survive empty contents, but not aborted first creation.
SELECT pg_my_temp_schema() = 0;
BEGIN;
SAVEPOINT before_namespace;
CREATE TEMP TABLE cleanup_first(id integer);
ROLLBACK TO before_namespace;
COMMIT;
SELECT pg_my_temp_schema() = 0;
CREATE TEMP VIEW cleanup_first AS SELECT 7 AS id;
SELECT pg_my_temp_schema() <> 0;
SELECT 'pg_temp'::regnamespace;
SELECT nspname::regnamespace::oid = pg_my_temp_schema()
FROM pg_namespace WHERE oid = pg_my_temp_schema();
SELECT nspowner = 10 FROM pg_namespace WHERE oid = pg_my_temp_schema();
DROP VIEW cleanup_first;
SELECT pg_my_temp_schema() <> 0;

CREATE TEMP TABLE cleanup_rows(id serial PRIMARY KEY, value integer);
INSERT INTO cleanup_rows(value) VALUES(7), (9);
CREATE TEMP VIEW cleanup_view AS SELECT id, value FROM cleanup_rows;
CREATE TEMP SEQUENCE cleanup_sequence;
CREATE STATISTICS public.cleanup_statistics ON id, value FROM cleanup_rows;
BEGIN;
SAVEPOINT before_cleanup;
DISCARD TEMP;
SELECT to_regclass('cleanup_rows') IS NULL,
       to_regclass('cleanup_view') IS NULL,
       to_regclass('cleanup_sequence') IS NULL,
       pg_my_temp_schema() <> 0;
SELECT count(*) FROM pg_statistic_ext WHERE stxname = 'cleanup_statistics';
ROLLBACK TO before_cleanup;
SELECT count(*) FROM cleanup_rows;
SELECT count(*) FROM cleanup_view;
SELECT count(*) FROM pg_statistic_ext WHERE stxname = 'cleanup_statistics';
COMMIT;

CREATE SEQUENCE cleanup_durable_counter START 10;
SELECT nextval('cleanup_durable_counter');
DISCARD PLANS;
SELECT currval('cleanup_durable_counter');
DISCARD SEQUENCES;
SELECT currval('cleanup_durable_counter');
SELECT nextval('cleanup_durable_counter');
PREPARE cleanup_prepared AS SELECT 7;
SET application_name = 'cleanup-session';
SELECT pg_advisory_lock(193456);
DISCARD ALL;
SELECT to_regclass('cleanup_rows') IS NULL,
       to_regclass('cleanup_view') IS NULL,
       to_regclass('cleanup_sequence') IS NULL,
       pg_my_temp_schema() <> 0;
SELECT count(*) FROM pg_statistic_ext WHERE stxname = 'cleanup_statistics';
SELECT current_setting('application_name') <> 'cleanup-session';
EXECUTE cleanup_prepared;
SELECT pg_advisory_unlock(193456);
SELECT nextval('cleanup_durable_counter');
DROP SEQUENCE cleanup_durable_counter;

-- A table drop also removes its statistics named in a surviving schema.
CREATE SCHEMA cleanup_owner;
CREATE TABLE cleanup_owner.rows(id integer, value integer);
CREATE STATISTICS public.cleanup_outside ON id, value FROM cleanup_owner.rows;
BEGIN;
SAVEPOINT before_schema;
DROP SCHEMA cleanup_owner CASCADE;
SELECT count(*) FROM pg_statistic_ext WHERE stxname = 'cleanup_outside';
ROLLBACK TO before_schema;
COMMIT;
SELECT count(*) FROM pg_statistic_ext WHERE stxname = 'cleanup_outside';
DROP SCHEMA cleanup_owner CASCADE;
SELECT count(*) FROM pg_statistic_ext WHERE stxname = 'cleanup_outside';
