DROP DATABASE IF EXISTS differential_database;

CREATE DATABASE differential_database
  WITH TEMPLATE template0 ENCODING 'UTF8' STRATEGY WAL_LOG
       LOCALE_PROVIDER libc LC_COLLATE 'C' LC_CTYPE 'C'
       ALLOW_CONNECTIONS true CONNECTION_LIMIT 3 IS_TEMPLATE false;
ALTER DATABASE differential_database WITH CONNECTION_LIMIT 2;
ALTER DATABASE differential_database SET application_name TO 'database-default';
COMMENT ON DATABASE differential_database IS 'differential database';
SELECT datname, pg_get_userbyid(datdba), encoding, datlocprovider,
       datistemplate, datallowconn, datconnlimit,
       shobj_description(oid, 'pg_database')
FROM pg_database
WHERE datname = 'differential_database';

\connect differential_database
SELECT current_database(), current_catalog, current_setting('application_name');
SELECT source, reset_val
FROM pg_settings
WHERE name = 'application_name';
CREATE TABLE database_local_row(id integer PRIMARY KEY, payload text);
INSERT INTO database_local_row VALUES (1, 'local');
SELECT * FROM database_local_row;

\connect postgres
SELECT count(*) FROM pg_class WHERE relname = 'database_local_row';
ALTER DATABASE differential_database RENAME TO differential_database_renamed;
ALTER DATABASE differential_database_renamed RESET application_name;
DROP DATABASE differential_database_renamed;

ALTER SYSTEM SET application_name = 'system-default';
SELECT pg_reload_conf();
SELECT setting, source, reset_val
FROM pg_settings
WHERE name = 'application_name';

CREATE TABLESPACE pg_default LOCATION '/duplicate-name-preflight';
ALTER TABLESPACE pg_default SET (random_page_cost = 1.25);
SELECT spcoptions::text FROM pg_tablespace WHERE spcname = 'pg_default';
ALTER TABLESPACE pg_default RESET (random_page_cost);
SELECT spcoptions IS NULL FROM pg_tablespace WHERE spcname = 'pg_default';
DROP TABLESPACE differential_missing_tablespace;
SET application_name = 'session-value';
SELECT setting, source, reset_val
FROM pg_settings
WHERE name = 'application_name';
DISCARD ALL;
SELECT setting, source, reset_val
FROM pg_settings
WHERE name = 'application_name';
ALTER SYSTEM RESET application_name;
SELECT pg_reload_conf();
SELECT setting, source, reset_val
FROM pg_settings
WHERE name = 'application_name';
