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
ALTER SCHEMA validity_probe OWNER TO postgres;
SELECT opaque();
SELECT 'noop schema owner', last_value FROM checks;

CREATE INDEX anchor_id_idx ON anchor(id);
SELECT opaque();
SELECT 'selected index create', last_value FROM checks;
ALTER INDEX anchor_id_idx RENAME TO anchor_id_renamed;
SELECT opaque();
SELECT 'selected index rename', last_value FROM checks;
ALTER INDEX anchor_id_renamed SET (fillfactor=80);
SELECT opaque();
SELECT 'selected index storage options', last_value FROM checks;
ALTER TABLE anchor OWNER TO postgres;
SELECT opaque();
SELECT 'selected table noop owner', last_value FROM checks;
ANALYZE anchor;
SELECT opaque();
SELECT 'selected table analyze', last_value FROM checks;
TRUNCATE anchor;
SELECT opaque();
SELECT 'selected table truncate empty', last_value FROM checks;
INSERT INTO anchor(id) VALUES (1);
SELECT opaque();
SELECT 'selected table insert after truncate', last_value FROM checks;
DROP INDEX anchor_id_renamed;
SELECT opaque();
SELECT 'selected index drop', last_value FROM checks;
CREATE INDEX unrelated_id_idx ON unrelated(id);
SELECT opaque();
SELECT 'unrelated index create', last_value FROM checks;
DROP INDEX unrelated_id_idx;

ALTER TABLE anchor ENABLE ROW LEVEL SECURITY;
SELECT opaque();
SELECT 'selected enable rls', last_value FROM checks;
CREATE POLICY anchor_allow ON anchor USING (true);
SELECT opaque();
SELECT 'selected policy create', last_value FROM checks;
ALTER POLICY anchor_allow ON anchor USING (true);
SELECT opaque();
SELECT 'selected policy same expression', last_value FROM checks;
DROP POLICY anchor_allow ON anchor;
SELECT opaque();
SELECT 'selected policy drop', last_value FROM checks;

CREATE RULE anchor_insert AS ON INSERT TO anchor DO ALSO NOTHING;
SELECT opaque();
SELECT 'selected rule create', last_value FROM checks;
DROP RULE anchor_insert ON anchor;
SELECT opaque();
SELECT 'selected rule drop', last_value FROM checks;

CREATE ROLE validity_reader;
GRANT USAGE ON SCHEMA validity_probe TO validity_reader;
GRANT SELECT ON anchor TO validity_reader;
GRANT USAGE, SELECT ON SEQUENCE checks TO validity_reader;
SELECT opaque();
SELECT 'rls plan after role creation', last_value FROM checks;
ALTER ROLE validity_reader LOGIN;
SELECT opaque();
SELECT 'rls plan after unrelated role alteration', last_value FROM checks;
ALTER ROLE validity_reader LOGIN;
SELECT opaque();
SELECT 'rls plan after repeated role alteration', last_value FROM checks;
SET ROLE validity_reader;
SELECT opaque();
SELECT 'rls plan different role', last_value FROM checks;
RESET ROLE;
SELECT opaque();
SELECT 'rls plan original role', last_value FROM checks;

ALTER TABLE anchor DISABLE ROW LEVEL SECURITY;
SELECT opaque();
SELECT 'plain plan after disabling rls', last_value FROM checks;
SET ROLE validity_reader;
SELECT opaque();
SELECT 'plain plan different role', last_value FROM checks;
RESET ROLE;
ALTER ROLE validity_reader NOLOGIN;
SELECT opaque();
SELECT 'plain plan after role alteration', last_value FROM checks;

-- Cross-backend invalidation is checked by routine-input-oracle.py using two
-- persistent psql sessions, without adding a PostgreSQL extension.

-- PL/pgSQL's backend cache also specializes input collation and polymorphism.
CREATE SCHEMA runtime_specialization_probe;
SET search_path=runtime_specialization_probe,public;
CREATE SEQUENCE specialization_checks;
CREATE TYPE specialization_choice AS ENUM('a');
CREATE DOMAIN specialization_checked AS specialization_choice
  CHECK(nextval('specialization_checks')>0);
CREATE FUNCTION pl_collated(input text DEFAULT ('a' COLLATE "C"))
  RETURNS specialization_checked[] LANGUAGE plpgsql
  AS $$BEGIN RETURN '{a}'::specialization_checked[]; END$$;
SELECT pl_collated();
SELECT 'pl first default c',last_value FROM specialization_checks;
SELECT pl_collated('a' COLLATE "C");
SELECT 'pl same explicit c',last_value FROM specialization_checks;
SELECT pl_collated('a' COLLATE "POSIX");
SELECT 'pl separate posix',last_value FROM specialization_checks;
SELECT pl_collated();
SELECT 'pl reused c',last_value FROM specialization_checks;
CREATE FUNCTION pl_polymorphic(input anyelement)
  RETURNS specialization_checked[] LANGUAGE plpgsql
  AS $$BEGIN RETURN '{a}'::specialization_checked[]; END$$;
ALTER SEQUENCE specialization_checks RESTART WITH 1;
SELECT pl_polymorphic(1);
SELECT 'pl first integer',last_value FROM specialization_checks;
SELECT pl_polymorphic(2);
SELECT 'pl same integer',last_value FROM specialization_checks;
SELECT pl_polymorphic('a'::text);
SELECT 'pl separate text',last_value FROM specialization_checks;
SELECT pl_polymorphic(3);
SELECT 'pl reused integer',last_value FROM specialization_checks;
CREATE FUNCTION sql_collation_inner(input text) RETURNS specialization_checked[]
  LANGUAGE sql SET search_path=runtime_specialization_probe,public
  AS $$SELECT '{a}'::specialization_checked[]$$;
CREATE FUNCTION sql_collation_outer(input text) RETURNS specialization_checked[]
  LANGUAGE sql SET search_path=runtime_specialization_probe,public
  AS $$SELECT sql_collation_inner(input)$$;
CREATE FUNCTION pl_collation_outer(input text) RETURNS specialization_checked[]
  LANGUAGE plpgsql AS $$BEGIN RETURN sql_collation_inner(input); END$$;
ALTER SEQUENCE specialization_checks RESTART WITH 1;
SELECT sql_collation_outer('a' COLLATE "C");
SELECT 'sql formal first c',last_value FROM specialization_checks;
SELECT sql_collation_outer('a' COLLATE "POSIX");
SELECT 'sql formal separate posix',last_value FROM specialization_checks;
SELECT sql_collation_outer('a' COLLATE "C");
SELECT 'sql formal reused c',last_value FROM specialization_checks;
DISCARD PLANS;
ALTER SEQUENCE specialization_checks RESTART WITH 1;
SELECT pl_collation_outer('a' COLLATE "C");
SELECT 'pl formal first c',last_value FROM specialization_checks;
SELECT pl_collation_outer('a' COLLATE "POSIX");
SELECT 'pl formal separate posix',last_value FROM specialization_checks;
SELECT pl_collation_outer('a' COLLATE "C");
SELECT 'pl formal reused c',last_value FROM specialization_checks;
RESET search_path;
DROP SCHEMA runtime_specialization_probe CASCADE;

SET search_path=validity_probe,public;
SELECT opaque();
SELECT 'prepare baseline', last_value FROM checks;
BEGIN;
CREATE SCHEMA oracle_prepared_committed;
SELECT opaque();
SELECT 'prepare creator before prepare', last_value FROM checks;
PREPARE TRANSACTION 'oracle-plan-commit';
SELECT opaque();
SELECT 'prepare creator after prepare', last_value FROM checks;
COMMIT PREPARED 'oracle-plan-commit';
SELECT opaque();
SELECT 'prepare creator after commit prepared', last_value FROM checks;

BEGIN;
CREATE SCHEMA oracle_prepared_rolled_back;
SELECT opaque();
SELECT 'prepare creator before rollback prepare', last_value FROM checks;
PREPARE TRANSACTION 'oracle-plan-rollback';
SELECT opaque();
SELECT 'prepare creator before rollback prepared', last_value FROM checks;
ROLLBACK PREPARED 'oracle-plan-rollback';
SELECT opaque();
SELECT 'prepare creator after rollback prepared', last_value FROM checks;

BEGIN;
INSERT INTO anchor(id) VALUES (2);
PREPARE TRANSACTION 'oracle-plan-data';
SELECT opaque();
SELECT 'prepare creator after data-only prepare', last_value FROM checks;
ROLLBACK PREPARED 'oracle-plan-data';
SELECT opaque();
SELECT 'prepare creator after data-only rollback prepared', last_value FROM checks;

CREATE SEQUENCE reentrant_checks;
CREATE TYPE reentrant_choice AS ENUM('a');
CREATE FUNCTION reentrant_allowed(value reentrant_choice) RETURNS boolean LANGUAGE plpgsql AS $$
BEGIN
  PERFORM nextval('validity_probe.reentrant_checks');
  CREATE SCHEMA IF NOT EXISTS reentrant_created;
  RETURN true;
END $$;
CREATE DOMAIN reentrant_checked AS reentrant_choice
  CHECK(validity_probe.reentrant_allowed(VALUE));
SET check_function_bodies=off;
CREATE FUNCTION reentrant_body() RETURNS validity_probe.reentrant_checked[]
  LANGUAGE sql SET search_path=validity_probe,public AS
  $$ SELECT '{a}'::reentrant_checked[] $$;
SET check_function_bodies=on;
SELECT reentrant_body(),reentrant_body();
SELECT 'reentrant checked namespace first pair',last_value FROM reentrant_checks;
DROP SCHEMA reentrant_created;
SELECT reentrant_body(),reentrant_body();
SELECT 'reentrant checked namespace second pair',last_value FROM reentrant_checks;
