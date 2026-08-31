-- Replica identity is typed catalog state that controls logical UPDATE and
-- DELETE tuple shape; the selected index remains visible across changes.
DROP TABLE IF EXISTS replica_identity_rows;

CREATE TABLE replica_identity_rows (
  id integer PRIMARY KEY,
  alternate integer NOT NULL,
  payload text
);
CREATE UNIQUE INDEX replica_identity_alternate ON replica_identity_rows (alternate);

SELECT relreplident FROM pg_class WHERE oid = 'replica_identity_rows'::regclass;
SELECT indisreplident FROM pg_index
 WHERE indexrelid = 'replica_identity_alternate'::regclass;

ALTER TABLE replica_identity_rows REPLICA IDENTITY FULL;
SELECT relreplident FROM pg_class WHERE oid = 'replica_identity_rows'::regclass;

ALTER TABLE replica_identity_rows REPLICA IDENTITY USING INDEX replica_identity_alternate;
SELECT relreplident FROM pg_class WHERE oid = 'replica_identity_rows'::regclass;
SELECT indisreplident FROM pg_index
 WHERE indexrelid = 'replica_identity_alternate'::regclass;

BEGIN;
ALTER TABLE replica_identity_rows REPLICA IDENTITY NOTHING;
SELECT relreplident FROM pg_class WHERE oid = 'replica_identity_rows'::regclass;
ROLLBACK;
SELECT relreplident FROM pg_class WHERE oid = 'replica_identity_rows'::regclass;

ALTER TABLE replica_identity_rows REPLICA IDENTITY DEFAULT;
SELECT relreplident FROM pg_class WHERE oid = 'replica_identity_rows'::regclass;
SELECT indisreplident FROM pg_index
 WHERE indexrelid = 'replica_identity_alternate'::regclass;

-- Dropping an identity-index column drops the index but PostgreSQL keeps the
-- relation mode; without a selected key, UPDATE and DELETE are not publishable.
CREATE TABLE replica_identity_dropped_index (id integer, alternate integer NOT NULL);
CREATE UNIQUE INDEX replica_identity_dropped_index_alternate
  ON replica_identity_dropped_index (alternate);
ALTER TABLE replica_identity_dropped_index
  REPLICA IDENTITY USING INDEX replica_identity_dropped_index_alternate;
ALTER TABLE replica_identity_dropped_index DROP COLUMN alternate;
SELECT relreplident FROM pg_class
 WHERE oid = 'replica_identity_dropped_index'::regclass;
SELECT count(*) FROM pg_index
 WHERE indrelid = 'replica_identity_dropped_index'::regclass
   AND indisreplident;
DROP TABLE replica_identity_dropped_index;

DROP TABLE replica_identity_rows;
