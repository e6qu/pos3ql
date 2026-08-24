DROP VIEW IF EXISTS rls_diff_definer;
DROP VIEW IF EXISTS rls_diff_invoker;
DROP TABLE IF EXISTS rls_diff_source;
DROP TABLE IF EXISTS rls_diff_target;
DROP ROLE IF EXISTS rls_diff_owner;
DROP ROLE IF EXISTS rls_diff_client;
DROP ROLE IF EXISTS rls_diff_blind;

CREATE ROLE rls_diff_owner;
CREATE ROLE rls_diff_client;
CREATE ROLE rls_diff_blind;
GRANT CREATE ON SCHEMA public TO rls_diff_owner;
SET ROLE rls_diff_owner;
CREATE TABLE rls_diff_target (id integer PRIMARY KEY, tenant text, payload text);
INSERT INTO rls_diff_target VALUES
  (1, 'rls_diff_client', 'client'), (2, 'other', 'other');
CREATE TABLE rls_diff_source (id integer, tenant text, payload text);
INSERT INTO rls_diff_source VALUES
  (1, 'rls_diff_client', 'updated'), (3, 'rls_diff_client', 'inserted');
ALTER TABLE rls_diff_target ENABLE ROW LEVEL SECURITY;
CREATE POLICY rls_diff_bad_aggregate ON rls_diff_target USING (count(*) > 0);
CREATE POLICY rls_diff_bad_window ON rls_diff_target
  USING (row_number() OVER () > 0);
CREATE POLICY rls_diff_bad_set ON rls_diff_target
  USING (generate_series(1, 2) > 0);
CREATE POLICY rls_diff_rows ON rls_diff_target FOR ALL TO rls_diff_client
  USING (tenant = 'rls_diff_client') WITH CHECK (tenant = 'rls_diff_client');
GRANT SELECT, INSERT, UPDATE, DELETE ON rls_diff_target TO rls_diff_client;
GRANT SELECT ON rls_diff_source TO rls_diff_client;
CREATE VIEW rls_diff_definer AS SELECT id, tenant, payload FROM rls_diff_target;
CREATE VIEW rls_diff_invoker WITH (security_invoker=true) AS
  SELECT id, tenant, payload FROM rls_diff_target;
GRANT SELECT ON rls_diff_definer, rls_diff_invoker TO rls_diff_client;
RESET ROLE;

SET ROLE rls_diff_client;
SELECT id FROM rls_diff_target ORDER BY id;
SELECT id FROM rls_diff_definer ORDER BY id;
SELECT id FROM rls_diff_invoker ORDER BY id;
PREPARE rls_diff_saved AS SELECT id FROM rls_diff_target ORDER BY id;
EXECUTE rls_diff_saved;
BEGIN;
DECLARE rls_diff_cursor CURSOR FOR SELECT id FROM rls_diff_target ORDER BY id;
FETCH ALL FROM rls_diff_cursor;
COMMIT;
MERGE INTO rls_diff_target AS target USING rls_diff_source AS source
  ON target.id = source.id
  WHEN MATCHED THEN UPDATE SET payload = source.payload
  WHEN NOT MATCHED THEN INSERT (id, tenant, payload)
    VALUES (source.id, source.tenant, source.payload);
SELECT id, payload FROM rls_diff_target ORDER BY id;
COPY rls_diff_target (id, tenant) TO STDOUT;
RESET ROLE;

SELECT polname, polcmd, polpermissive,
       polroles = ARRAY['rls_diff_client'::regrole]::oid[] AS exact_roles,
       polqual IS NOT NULL, polwithcheck IS NOT NULL
  FROM pg_policy
 WHERE polrelid = 'rls_diff_target'::regclass;
SELECT policyname, permissive, roles, cmd,
       qual IS NOT NULL AS has_qual, with_check IS NOT NULL AS has_with_check
  FROM pg_policies
 WHERE tablename = 'rls_diff_target';
SELECT relrowsecurity, relforcerowsecurity
  FROM pg_class WHERE oid = 'rls_diff_target'::regclass;
SELECT reloptions FROM pg_class WHERE oid = 'rls_diff_invoker'::regclass;

SET ROLE rls_diff_owner;
ALTER TABLE rls_diff_target FORCE ROW LEVEL SECURITY;
SELECT count(*) FROM rls_diff_target;
ALTER TABLE rls_diff_target NO FORCE ROW LEVEL SECURITY;
ALTER POLICY rls_diff_rows ON rls_diff_target
  USING (tenant = 'rls_diff_client' AND id > 1);
RESET ROLE;
SET ROLE rls_diff_client;
SELECT id FROM rls_diff_target ORDER BY id;
RESET ROLE;

CREATE TABLE rls_diff_composition (id integer PRIMARY KEY, note text);
INSERT INTO rls_diff_composition VALUES (1, 'one'), (2, 'two');
ALTER TABLE rls_diff_composition ENABLE ROW LEVEL SECURITY;
CREATE POLICY rls_diff_select ON rls_diff_composition
  FOR SELECT TO rls_diff_client USING (id = 1);
CREATE POLICY rls_diff_insert ON rls_diff_composition
  FOR INSERT TO rls_diff_client WITH CHECK (true);
CREATE POLICY rls_diff_update ON rls_diff_composition
  FOR UPDATE TO rls_diff_client USING (true) WITH CHECK (true);
CREATE POLICY rls_diff_delete ON rls_diff_composition
  FOR DELETE TO rls_diff_client USING (true);
CREATE POLICY rls_diff_blind_update ON rls_diff_composition
  FOR UPDATE TO rls_diff_blind USING (true) WITH CHECK (true);
GRANT SELECT, INSERT, UPDATE, DELETE ON rls_diff_composition TO rls_diff_client;
GRANT UPDATE ON rls_diff_composition TO rls_diff_blind;
SET ROLE rls_diff_blind;
UPDATE rls_diff_composition SET note = (SELECT 'blind');
RESET ROLE;
SET ROLE rls_diff_client;
INSERT INTO rls_diff_composition VALUES (2, 'conflict')
  ON CONFLICT (id) DO UPDATE SET note = 'conflict';
UPDATE rls_diff_composition SET note = 'visible' RETURNING id;
DELETE FROM rls_diff_composition RETURNING id;
RESET ROLE;
SELECT id, note FROM rls_diff_composition;
DROP TABLE rls_diff_composition;

CREATE TABLE rls_diff_dep_source (id integer, tenant text, keep text);
CREATE TABLE rls_diff_dep_target (id integer);
ALTER TABLE rls_diff_dep_source ENABLE ROW LEVEL SECURITY;
ALTER TABLE rls_diff_dep_target ENABLE ROW LEVEL SECURITY;
CREATE POLICY rls_diff_dep_source_policy ON rls_diff_dep_source
  USING (tenant = 'client');
CREATE POLICY rls_diff_dep_target_policy ON rls_diff_dep_target
  USING (EXISTS (SELECT 1 FROM rls_diff_dep_source WHERE tenant = 'client'));
CREATE POLICY rls_diff_dep_unrelated ON rls_diff_dep_target USING (id > 0);
CREATE VIEW rls_diff_dep_view AS SELECT tenant FROM rls_diff_dep_source;
CREATE VIEW rls_diff_dep_view_chain AS SELECT tenant FROM rls_diff_dep_view;
ALTER TABLE rls_diff_dep_source DROP COLUMN tenant;
SELECT count(*) FROM pg_policy
 WHERE polname LIKE 'rls_diff_dep_%';
BEGIN;
ALTER TABLE rls_diff_dep_source DROP COLUMN tenant CASCADE;
ROLLBACK;
SELECT count(*) FROM pg_policy
 WHERE polname LIKE 'rls_diff_dep_%';
SELECT tenant FROM rls_diff_dep_view_chain;
ALTER TABLE rls_diff_dep_source DROP COLUMN IF EXISTS missing;
ALTER TABLE rls_diff_dep_source DROP COLUMN tenant CASCADE;
SELECT polname FROM pg_policy
 WHERE polrelid = 'rls_diff_dep_target'::regclass ORDER BY polname;
SELECT column_name FROM information_schema.columns
 WHERE table_name = 'rls_diff_dep_source' ORDER BY ordinal_position;
SELECT * FROM rls_diff_dep_view;
DROP TABLE rls_diff_dep_source;
DROP TABLE rls_diff_dep_target;

CREATE TABLE rls_diff_object_target (id integer);
ALTER TABLE rls_diff_object_target ENABLE ROW LEVEL SECURITY;
CREATE TABLE rls_diff_object_source (id integer);
CREATE POLICY rls_diff_object_table ON rls_diff_object_target
  USING (EXISTS (SELECT 1 FROM rls_diff_object_source));
DROP TABLE rls_diff_object_source;
DROP TABLE rls_diff_object_source CASCADE;
SELECT count(*) FROM pg_policy WHERE polname = 'rls_diff_object_table';
CREATE VIEW rls_diff_object_view AS SELECT id FROM rls_diff_object_target;
CREATE POLICY rls_diff_object_view_policy ON rls_diff_object_target
  USING (EXISTS (SELECT 1 FROM rls_diff_object_view));
DROP VIEW rls_diff_object_view;
DROP VIEW rls_diff_object_view CASCADE;
SELECT count(*) FROM pg_policy WHERE polname = 'rls_diff_object_view_policy';
CREATE FUNCTION rls_diff_object_function(integer) RETURNS boolean
  LANGUAGE SQL AS 'SELECT $1 > 0';
CREATE POLICY rls_diff_object_function_policy ON rls_diff_object_target
  USING (rls_diff_object_function(id));
DROP FUNCTION rls_diff_object_function(integer);
DROP FUNCTION rls_diff_object_function(integer) CASCADE;
SELECT count(*) FROM pg_policy WHERE polname = 'rls_diff_object_function_policy';
CREATE SEQUENCE rls_diff_object_sequence;
CREATE POLICY rls_diff_object_sequence_policy ON rls_diff_object_target
  USING (nextval('rls_diff_object_sequence') > 0);
DROP SEQUENCE rls_diff_object_sequence;
DROP SEQUENCE rls_diff_object_sequence CASCADE;
SELECT count(*) FROM pg_policy WHERE polname = 'rls_diff_object_sequence_policy';
CREATE DOMAIN rls_diff_object_domain AS integer;
CREATE POLICY rls_diff_object_domain_policy ON rls_diff_object_target
  USING (id::rls_diff_object_domain > 0);
DROP DOMAIN rls_diff_object_domain;
DROP DOMAIN rls_diff_object_domain CASCADE;
SELECT count(*) FROM pg_policy WHERE polname = 'rls_diff_object_domain_policy';
CREATE TYPE rls_diff_object_enum AS ENUM ('ok');
CREATE POLICY rls_diff_object_enum_policy ON rls_diff_object_target
  USING ('ok'::rls_diff_object_enum = 'ok'::rls_diff_object_enum);
DROP TYPE rls_diff_object_enum;
DROP TYPE rls_diff_object_enum CASCADE;
SELECT count(*) FROM pg_policy WHERE polname = 'rls_diff_object_enum_policy';
CREATE ROLE rls_diff_owned_role;
GRANT CREATE ON SCHEMA public TO rls_diff_owned_role;
SET ROLE rls_diff_owned_role;
CREATE SEQUENCE rls_diff_owned_sequence;
RESET ROLE;
CREATE POLICY rls_diff_owned_policy ON rls_diff_object_target
  USING (nextval('rls_diff_owned_sequence') > 0);
DROP OWNED BY rls_diff_owned_role RESTRICT;
DROP OWNED BY rls_diff_owned_role CASCADE;
SELECT count(*) FROM pg_policy WHERE polname = 'rls_diff_owned_policy';
DROP ROLE rls_diff_owned_role;
CREATE SCHEMA rls_diff_policy_schema;
CREATE SEQUENCE rls_diff_policy_schema.sequence_dependency;
CREATE POLICY rls_diff_schema_policy ON rls_diff_object_target
  USING (nextval('rls_diff_policy_schema.sequence_dependency') > 0);
DROP SCHEMA rls_diff_policy_schema RESTRICT;
DROP SCHEMA rls_diff_policy_schema CASCADE;
SELECT count(*) FROM pg_policy WHERE polname = 'rls_diff_schema_policy';
DROP TABLE rls_diff_object_target;

ALTER ROLE rls_diff_client RENAME TO rls_diff_client_renamed;
SET ROLE rls_diff_client_renamed;
SELECT id FROM rls_diff_target ORDER BY id;
RESET ROLE;
SELECT roles FROM pg_policies
 WHERE tablename = 'rls_diff_target' AND policyname = 'rls_diff_rows';
ALTER TABLE rls_diff_target RENAME tenant TO account_name;
ALTER TABLE rls_diff_target RENAME TO rls_diff_target_renamed;
SET ROLE rls_diff_client_renamed;
SELECT id FROM rls_diff_target_renamed ORDER BY id;
RESET ROLE;
SELECT tablename, qual IS NOT NULL, with_check IS NOT NULL
  FROM pg_policies WHERE policyname = 'rls_diff_rows';

DROP VIEW rls_diff_definer;
DROP VIEW rls_diff_invoker;
DROP TABLE rls_diff_source;
DROP TABLE rls_diff_target_renamed;
DROP ROLE rls_diff_owner;
DROP ROLE rls_diff_client_renamed;
DROP ROLE rls_diff_blind;
