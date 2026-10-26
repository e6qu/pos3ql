CREATE SCHEMA identity_scope;
CREATE TABLE identity_saved(name text, relation oid, row_type oid, array_type oid);
CREATE TABLE identity_scope.orders(id integer PRIMARY KEY, label text UNIQUE);
CREATE INDEX orders_label_index ON identity_scope.orders(label);
CREATE VIEW identity_scope.report AS SELECT id FROM identity_scope.orders;
CREATE SEQUENCE identity_scope.ticket START WITH 41;
CREATE MATERIALIZED VIEW identity_scope.totals AS SELECT 1 AS total;
INSERT INTO identity_saved
SELECT c.relname, c.oid, c.reltype, t.typarray
  FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
  LEFT JOIN pg_type t ON t.oid = c.reltype
 WHERE n.nspname = 'identity_scope';
SELECT count(*) > 0 FROM identity_saved;
ALTER TABLE identity_scope.orders RENAME TO purchases;
SELECT count(*) = 0 FROM identity_saved s
  LEFT JOIN pg_class c ON c.oid = s.relation WHERE c.oid IS NULL;
ALTER TABLE identity_scope.purchases RENAME TO orders;
DROP VIEW identity_scope.report;
DROP TABLE identity_scope.orders;
DROP SEQUENCE identity_scope.ticket;
DROP MATERIALIZED VIEW identity_scope.totals;
CREATE TABLE identity_scope.orders(id integer PRIMARY KEY, label text UNIQUE);
CREATE INDEX orders_label_index ON identity_scope.orders(label);
CREATE VIEW identity_scope.report AS SELECT id FROM identity_scope.orders;
CREATE SEQUENCE identity_scope.ticket START WITH 81;
CREATE MATERIALIZED VIEW identity_scope.totals AS SELECT 2 AS total;
SELECT count(*) = 0 FROM pg_class c JOIN identity_saved s ON c.oid = s.relation;
SELECT count(*) = 0 FROM pg_type t JOIN identity_saved s ON t.oid = s.row_type;
SELECT count(*) = 0 FROM pg_type t JOIN identity_saved s ON t.oid = s.array_type;
SELECT count(*) = 0 FROM identity_saved
 WHERE has_table_privilege(relation, 'SELECT') IS NOT NULL;
SELECT count(*) = 0 FROM identity_saved
 WHERE pg_table_is_visible(relation) IS NOT NULL;
SELECT nextval('identity_scope.ticket');

CREATE TABLE identity_keys(a integer UNIQUE, b integer UNIQUE);
CREATE TABLE identity_key_saved AS
SELECT oid FROM pg_class WHERE oid = 'identity_keys_b_key'::regclass;
ALTER TABLE identity_keys DROP CONSTRAINT identity_keys_a_key;
SELECT oid = 'identity_keys_b_key'::regclass FROM identity_key_saved;
ALTER TABLE identity_keys RENAME CONSTRAINT identity_keys_b_key TO identity_kept_key;
SELECT oid = 'identity_kept_key'::regclass FROM identity_key_saved;
BEGIN;
SAVEPOINT keep_index;
ALTER TABLE identity_keys DROP CONSTRAINT identity_kept_key;
ALTER TABLE identity_keys ADD CONSTRAINT identity_kept_key UNIQUE(b);
SELECT oid <> 'identity_kept_key'::regclass FROM identity_key_saved;
ROLLBACK TO SAVEPOINT keep_index;
SELECT oid = 'identity_kept_key'::regclass FROM identity_key_saved;
COMMIT;
ALTER TABLE identity_keys DROP CONSTRAINT identity_kept_key;
ALTER TABLE identity_keys ADD CONSTRAINT identity_kept_key UNIQUE(b);
SELECT oid <> 'identity_kept_key'::regclass FROM identity_key_saved;
DROP TABLE identity_keys, identity_key_saved;
DROP VIEW identity_scope.report;
DROP TABLE identity_scope.orders;
DROP SEQUENCE identity_scope.ticket;
DROP MATERIALIZED VIEW identity_scope.totals;
DROP SCHEMA identity_scope;
DROP TABLE identity_saved;
