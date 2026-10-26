CREATE SCHEMA type_identity_scope;
CREATE DOMAIN type_identity_scope.quantity AS integer CHECK (VALUE > 0);
CREATE TYPE type_identity_scope.status AS ENUM ('new', 'paid');
CREATE TYPE type_identity_scope.item AS (id integer, status type_identity_scope.status, quantity type_identity_scope.quantity);
CREATE TABLE saved_type_identities AS
SELECT t.typname AS name, t.oid AS identity, t.typarray AS array_identity, t.typrelid AS relation
  FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace
 WHERE n.nspname = 'type_identity_scope' AND t.typname IN ('quantity', 'status', 'item');
CREATE TABLE saved_enum_identities AS
SELECT e.oid AS identity, e.enumlabel AS label
  FROM pg_enum e WHERE e.enumtypid = 'type_identity_scope.status'::regtype;
ALTER TYPE type_identity_scope.status RENAME VALUE 'new' TO 'pending';
ALTER TYPE type_identity_scope.status ADD VALUE 'refunded' BEFORE 'paid';
SELECT count(*) = 0 FROM saved_enum_identities s
  LEFT JOIN pg_enum e ON e.oid = s.identity WHERE e.oid IS NULL;
ALTER DOMAIN type_identity_scope.quantity RENAME TO amount;
ALTER TYPE type_identity_scope.status RENAME TO payment_status;
ALTER TYPE type_identity_scope.item RENAME TO order_item;
SELECT count(*) = 0 FROM saved_type_identities s
  LEFT JOIN pg_type t ON t.oid = s.identity WHERE t.oid IS NULL;
SELECT count(*) = 0 FROM saved_type_identities s
  LEFT JOIN pg_type t ON t.oid = s.array_identity WHERE t.oid IS NULL;
DROP TYPE type_identity_scope.order_item, type_identity_scope.payment_status;
DROP DOMAIN type_identity_scope.amount;
CREATE DOMAIN type_identity_scope.quantity AS integer CHECK (VALUE > 0);
CREATE TYPE type_identity_scope.status AS ENUM ('new', 'paid');
CREATE TYPE type_identity_scope.item AS (id integer, status type_identity_scope.status, quantity type_identity_scope.quantity);
SELECT count(*) = 0 FROM pg_type t JOIN saved_type_identities s ON t.oid = s.identity OR t.oid = s.array_identity;
SELECT count(*) = 0 FROM pg_enum e JOIN saved_enum_identities s ON e.oid = s.identity;
SELECT count(*) = 0 FROM saved_type_identities WHERE has_type_privilege(identity, 'USAGE') IS NOT NULL;
SELECT count(*) = 0 FROM saved_type_identities WHERE pg_type_is_visible(identity) IS NOT NULL;
SELECT count(*) = 0 FROM saved_type_identities s JOIN pg_class c ON c.oid = s.relation WHERE s.relation <> 0;
BEGIN;
SAVEPOINT before_type;
CREATE TYPE type_identity_scope.discarded AS ENUM ('one');
CREATE TEMP TABLE discarded_type_identity AS SELECT 'type_identity_scope.discarded'::regtype::oid AS identity;
ROLLBACK TO SAVEPOINT before_type;
COMMIT;
DROP TYPE type_identity_scope.item, type_identity_scope.status;
DROP DOMAIN type_identity_scope.quantity;
DROP SCHEMA type_identity_scope;
DROP TABLE saved_type_identities, saved_enum_identities;
