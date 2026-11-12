-- Foreign-data wrappers, servers, user mappings and foreign tables are
-- pg_depend rows, and DROP SERVER / DROP FOREIGN DATA WRAPPER walk them.
CREATE FOREIGN DATA WRAPPER w;
CREATE SERVER sv FOREIGN DATA WRAPPER w;
CREATE SERVER sv2 FOREIGN DATA WRAPPER w;
CREATE USER MAPPING FOR CURRENT_USER SERVER sv;
CREATE USER MAPPING FOR PUBLIC SERVER sv;
CREATE FOREIGN TABLE ft (a int) SERVER sv;
CREATE VIEW fv AS SELECT a FROM ft;
SELECT pg_describe_object(classid, objid, objsubid) AS dependent, pg_describe_object(refclassid, refobjid, refobjsubid) AS referenced, deptype FROM pg_depend WHERE (refclassid IN ('pg_foreign_server'::regclass, 'pg_foreign_data_wrapper'::regclass) OR classid IN ('pg_foreign_server'::regclass, 'pg_user_mapping'::regclass)) AND objid >= 16384 ORDER BY 1, 2;
DROP SERVER sv;
DROP SERVER sv, sv2;
DROP FOREIGN DATA WRAPPER w;
BEGIN;
DROP SERVER sv CASCADE;
ROLLBACK;
DROP FOREIGN DATA WRAPPER w CASCADE;
SELECT count(*) FROM pg_foreign_server;
SELECT count(*) FROM pg_class WHERE relname IN ('ft', 'fv');
