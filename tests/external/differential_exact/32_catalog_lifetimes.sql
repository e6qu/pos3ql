CREATE TABLE lifetime_rows (a integer, b integer);
CREATE FOREIGN DATA WRAPPER lifetime_wrapper NO HANDLER NO VALIDATOR;
CREATE SERVER lifetime_server FOREIGN DATA WRAPPER lifetime_wrapper;
CREATE USER MAPPING FOR PUBLIC SERVER lifetime_server;
CREATE STATISTICS lifetime_statistics ON a, b FROM lifetime_rows;
CREATE PUBLICATION lifetime_publication FOR TABLE lifetime_rows;
CREATE SUBSCRIPTION lifetime_subscription CONNECTION 'host=127.0.0.1 port=5432 user=repl dbname=publisher sslmode=disable' PUBLICATION lifetime_publication WITH (connect = false, slot_name = NONE);
CREATE EXTENSION pos3ql_base;
CREATE VIEW lifetime_catalog AS
 SELECT 'wrapper' AS kind, oid AS identity FROM pg_foreign_data_wrapper WHERE fdwname = 'lifetime_wrapper'
 UNION ALL SELECT 'server', oid FROM pg_foreign_server WHERE srvname = 'lifetime_server'
 UNION ALL SELECT 'mapping', m.oid FROM pg_user_mapping m JOIN pg_foreign_server s ON s.oid = m.umserver WHERE s.srvname = 'lifetime_server'
 UNION ALL SELECT 'statistics', oid FROM pg_statistic_ext WHERE stxname = 'lifetime_statistics'
 UNION ALL SELECT 'publication', oid FROM pg_publication WHERE pubname = 'lifetime_publication'
 UNION ALL SELECT 'subscription', oid FROM pg_subscription WHERE subname = 'lifetime_subscription'
 UNION ALL SELECT 'extension', oid FROM pg_extension WHERE extname = 'pos3ql_base';
CREATE TABLE saved_catalog_lifetimes AS SELECT kind, identity FROM lifetime_catalog;
SELECT count(*) = 7 FROM saved_catalog_lifetimes s JOIN lifetime_catalog c ON c.kind = s.kind AND c.identity = s.identity;
ALTER FOREIGN DATA WRAPPER lifetime_wrapper OPTIONS (ADD note 'edited');
ALTER SERVER lifetime_server VERSION 'edited';
ALTER USER MAPPING FOR PUBLIC SERVER lifetime_server OPTIONS (ADD note 'edited');
ALTER STATISTICS lifetime_statistics SET STATISTICS 42;
ALTER PUBLICATION lifetime_publication SET (publish = 'insert');
ALTER SUBSCRIPTION lifetime_subscription SET (binary = true);
ALTER EXTENSION pos3ql_base SET SCHEMA public;
SELECT count(*) = 7 FROM saved_catalog_lifetimes s JOIN lifetime_catalog c ON c.kind = s.kind AND c.identity = s.identity;
BEGIN;
SAVEPOINT before_catalog_replacement;
DROP STATISTICS lifetime_statistics;
CREATE STATISTICS lifetime_statistics ON a, b FROM lifetime_rows;
DROP PUBLICATION lifetime_publication;
CREATE PUBLICATION lifetime_publication FOR TABLE lifetime_rows;
SELECT count(*) = 2 FROM saved_catalog_lifetimes s JOIN lifetime_catalog c ON c.kind = s.kind AND c.identity <> s.identity;
ROLLBACK TO SAVEPOINT before_catalog_replacement;
SELECT count(*) = 7 FROM saved_catalog_lifetimes s JOIN lifetime_catalog c ON c.kind = s.kind AND c.identity = s.identity;
COMMIT;
DROP EXTENSION pos3ql_base;
CREATE EXTENSION pos3ql_base;
DROP SUBSCRIPTION lifetime_subscription;
CREATE SUBSCRIPTION lifetime_subscription CONNECTION 'host=127.0.0.1 port=5432 user=repl dbname=publisher sslmode=disable' PUBLICATION lifetime_publication WITH (connect = false, slot_name = NONE);
DROP PUBLICATION lifetime_publication;
CREATE PUBLICATION lifetime_publication FOR TABLE lifetime_rows;
DROP STATISTICS lifetime_statistics;
CREATE STATISTICS lifetime_statistics ON a, b FROM lifetime_rows;
DROP USER MAPPING FOR PUBLIC SERVER lifetime_server;
CREATE USER MAPPING FOR PUBLIC SERVER lifetime_server;
SELECT count(*) = 5 FROM saved_catalog_lifetimes s JOIN lifetime_catalog c ON c.kind = s.kind AND c.identity <> s.identity;
DROP SERVER lifetime_server CASCADE;
CREATE SERVER lifetime_server FOREIGN DATA WRAPPER lifetime_wrapper;
CREATE USER MAPPING FOR PUBLIC SERVER lifetime_server;
SELECT count(*) = 6 FROM saved_catalog_lifetimes s JOIN lifetime_catalog c ON c.kind = s.kind AND c.identity <> s.identity;
DROP FOREIGN DATA WRAPPER lifetime_wrapper CASCADE;
CREATE FOREIGN DATA WRAPPER lifetime_wrapper NO HANDLER NO VALIDATOR;
CREATE SERVER lifetime_server FOREIGN DATA WRAPPER lifetime_wrapper;
CREATE USER MAPPING FOR PUBLIC SERVER lifetime_server;
SELECT count(*) = 7 FROM saved_catalog_lifetimes s JOIN lifetime_catalog c ON c.kind = s.kind AND c.identity <> s.identity;
SELECT count(*) = 0 FROM saved_catalog_lifetimes s JOIN lifetime_catalog c ON c.identity = s.identity;
DROP VIEW lifetime_catalog;
DROP TABLE saved_catalog_lifetimes;
DROP SUBSCRIPTION lifetime_subscription;
DROP PUBLICATION lifetime_publication;
DROP FOREIGN DATA WRAPPER lifetime_wrapper CASCADE;
DROP EXTENSION pos3ql_base;
DROP TABLE lifetime_rows;
