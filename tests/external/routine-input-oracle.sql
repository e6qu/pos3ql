\set ON_ERROR_STOP on
\pset format unaligned
\pset tuples_only on
CREATE SCHEMA validity_probe;
SET search_path = validity_probe, public;
CREATE SEQUENCE checks;
CREATE FUNCTION counted(text) RETURNS boolean LANGUAGE plpgsql AS $$
BEGIN PERFORM nextval('validity_probe.checks'); RETURN true; END $$;
CREATE DOMAIN checked_text AS text CHECK (validity_probe.counted(VALUE));
CREATE TABLE anchor (id integer);
INSERT INTO anchor VALUES (1);
CREATE FUNCTION opaque() RETURNS validity_probe.checked_text[]
LANGUAGE sql SET search_path = validity_probe, public AS
$$ SELECT '{ok}'::checked_text[] FROM anchor LIMIT 1 $$;
SELECT 'create', last_value FROM checks;
SELECT opaque();
SELECT 'first', last_value FROM checks;
SELECT opaque();
SELECT 'reuse', last_value FROM checks;

BEGIN;
ALTER TABLE anchor ADD COLUMN first_added text;
SELECT opaque();
SELECT 'own ddl before commit', last_value FROM checks;
COMMIT;
SELECT opaque();
SELECT 'own ddl after commit', last_value FROM checks;

BEGIN;
ALTER TABLE anchor ADD COLUMN rolled_back text;
SELECT opaque();
SELECT 'own ddl before rollback', last_value FROM checks;
ROLLBACK;
SELECT opaque();
SELECT 'own ddl after rollback', last_value FROM checks;

ALTER TABLE anchor ADD COLUMN IF NOT EXISTS first_added text;
SELECT opaque();
SELECT 'noop table ddl', last_value FROM checks;
DO $$BEGIN
  ALTER TABLE validity_probe.anchor ADD COLUMN first_added text;
EXCEPTION WHEN duplicate_column THEN NULL;
END $$;
SELECT opaque();
SELECT 'failed table ddl', last_value FROM checks;

CREATE TABLE unrelated (id integer);
SELECT opaque();
SELECT 'unrelated table create', last_value FROM checks;
ALTER TABLE unrelated ADD COLUMN extra text;
SELECT opaque();
SELECT 'unrelated table alter', last_value FROM checks;

BEGIN;
CREATE SCHEMA pending_namespace;
SELECT opaque();
SELECT 'namespace before commit', last_value FROM checks;
COMMIT;
SELECT opaque();
SELECT 'namespace after commit', last_value FROM checks;

BEGIN;
CREATE SCHEMA rolled_back_namespace;
SELECT opaque();
SELECT 'namespace before rollback', last_value FROM checks;
ROLLBACK;
SELECT opaque();
SELECT 'namespace after rollback', last_value FROM checks;
CREATE SCHEMA IF NOT EXISTS pending_namespace;
SELECT opaque();
SELECT 'noop namespace ddl', last_value FROM checks;

-- Cross-backend probe: with this backend open, another connection starts a
-- transaction, creates a new unrelated schema and waits before COMMIT. This
-- backend calls opaque() before and after that COMMIT. Then reverse the roles.
-- Neither operation locks anchor, so a blocked call cannot masquerade as a
-- successful invalidation test.
