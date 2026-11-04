CREATE TABLE reindex_deferred_identity(value integer UNIQUE DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE reindex_deferred_before AS
  SELECT c.oid AS constraint_oid, c.conindid AS index_oid FROM pg_constraint c
   WHERE c.conrelid = 'reindex_deferred_identity'::regclass AND c.contype = 'u';
SELECT 'before', i.indimmediate FROM pg_index i
  JOIN reindex_deferred_before b ON i.indexrelid = b.index_oid;
REINDEX INDEX CONCURRENTLY reindex_deferred_identity_value_key;
SELECT 'after', i.indimmediate, c.oid = b.constraint_oid, c.conindid <> b.index_oid
  FROM pg_constraint c JOIN pg_index i ON i.indexrelid = c.conindid, reindex_deferred_before b
 WHERE c.conrelid = 'reindex_deferred_identity'::regclass AND c.contype = 'u';
BEGIN;
INSERT INTO reindex_deferred_identity VALUES (1),(1);
SELECT count(*) FROM reindex_deferred_identity;
ROLLBACK;
CREATE TABLE reindex_deferred_after AS
  SELECT c.oid AS constraint_oid, c.conindid AS index_oid FROM pg_constraint c
   WHERE c.conrelid = 'reindex_deferred_identity'::regclass AND c.contype = 'u';
REINDEX INDEX reindex_deferred_identity_value_key;
SELECT 'ordinary', i.indimmediate, c.oid = b.constraint_oid, c.conindid = b.index_oid
  FROM pg_constraint c JOIN pg_index i ON i.indexrelid = c.conindid, reindex_deferred_after b
 WHERE c.conrelid = 'reindex_deferred_identity'::regclass AND c.contype = 'u';
DROP TABLE reindex_deferred_identity, reindex_deferred_before, reindex_deferred_after;
