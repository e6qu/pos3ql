CREATE SCHEMA enum_identity_scope;
CREATE TYPE enum_identity_scope.order_status AS ENUM ('new', 'paid');
CREATE TABLE saved_label_identities AS SELECT oid AS identity, enumlabel AS label FROM pg_enum
 WHERE enumtypid = 'enum_identity_scope.order_status'::regtype;
ALTER TYPE enum_identity_scope.order_status RENAME VALUE 'new' TO 'pending';
ALTER TYPE enum_identity_scope.order_status ADD VALUE 'refunded' BEFORE 'paid';
ALTER TYPE enum_identity_scope.order_status ADD VALUE 'voided';
INSERT INTO saved_label_identities SELECT oid, enumlabel FROM pg_enum
 WHERE enumtypid = 'enum_identity_scope.order_status'::regtype AND enumlabel IN ('refunded', 'voided');
SELECT count(*) = 4 FROM saved_label_identities s JOIN pg_enum e ON e.oid = s.identity;
SELECT count(*) = 1 FROM saved_label_identities s JOIN pg_enum e ON e.oid = s.identity
 WHERE s.label = 'new' AND e.enumlabel = 'pending';
SELECT count(*) = count(DISTINCT oid) FROM pg_enum
 WHERE enumtypid = 'enum_identity_scope.order_status'::regtype;
BEGIN;
SAVEPOINT keep_labels;
ALTER TYPE enum_identity_scope.order_status RENAME VALUE 'pending' TO 'discarded';
ALTER TYPE enum_identity_scope.order_status ADD VALUE 'cancelled';
ROLLBACK TO SAVEPOINT keep_labels;
COMMIT;
SELECT count(*) = 4 FROM saved_label_identities s JOIN pg_enum e ON e.oid = s.identity;
SELECT count(*) = 1 FROM saved_label_identities s JOIN pg_enum e ON e.oid = s.identity
 WHERE s.label = 'new' AND e.enumlabel = 'pending';
DROP TYPE enum_identity_scope.order_status;
CREATE TYPE enum_identity_scope.order_status AS ENUM ('new', 'paid');
SELECT count(*) = 0 FROM saved_label_identities s JOIN pg_enum e ON e.oid = s.identity;
DROP TYPE enum_identity_scope.order_status;
DROP SCHEMA enum_identity_scope;
DROP TABLE saved_label_identities;
