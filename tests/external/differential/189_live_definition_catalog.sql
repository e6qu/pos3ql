-- Composite and array row types follow transaction-visible relation names
-- and namespaces, including rollback and publication.
CREATE SCHEMA definition_owner_schema;
CREATE TABLE definition_owner_before (id integer);
SELECT t.typname, n.nspname FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace WHERE t.typname IN ('definition_owner_before', '_definition_owner_before', 'definition_owner_after', '_definition_owner_after') ORDER BY t.typname;
BEGIN;
ALTER TABLE definition_owner_before RENAME TO definition_owner_after;
SELECT t.typname, n.nspname FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace WHERE t.typname IN ('definition_owner_before', '_definition_owner_before', 'definition_owner_after', '_definition_owner_after') ORDER BY t.typname;
ALTER TABLE definition_owner_after SET SCHEMA definition_owner_schema;
SELECT t.typname, n.nspname FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace WHERE t.typname IN ('definition_owner_before', '_definition_owner_before', 'definition_owner_after', '_definition_owner_after') ORDER BY t.typname;
ROLLBACK;
SELECT t.typname, n.nspname FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace WHERE t.typname IN ('definition_owner_before', '_definition_owner_before', 'definition_owner_after', '_definition_owner_after') ORDER BY t.typname;
BEGIN;
ALTER TABLE definition_owner_before RENAME TO definition_owner_after;
ALTER TABLE definition_owner_after SET SCHEMA definition_owner_schema;
COMMIT;
SELECT t.typname, n.nspname FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace WHERE t.typname IN ('definition_owner_before', '_definition_owner_before', 'definition_owner_after', '_definition_owner_after') ORDER BY t.typname;
DROP TABLE definition_owner_schema.definition_owner_after;
DROP SCHEMA definition_owner_schema;
