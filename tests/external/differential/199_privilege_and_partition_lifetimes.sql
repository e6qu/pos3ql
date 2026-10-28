CREATE SCHEMA lifetime_privileges;

CREATE ROLE lifetime_member_group;

CREATE ROLE lifetime_member_first;

CREATE ROLE lifetime_member_second;

GRANT lifetime_member_group TO lifetime_member_first WITH ADMIN OPTION;

CREATE TABLE lifetime_saved_membership AS
  SELECT oid FROM pg_auth_members
  WHERE roleid = 'lifetime_member_group'::regrole
    AND member = 'lifetime_member_first'::regrole;

REVOKE ADMIN OPTION FOR lifetime_member_group FROM lifetime_member_first;

SELECT saved.oid = current_membership.oid
  FROM lifetime_saved_membership saved, pg_auth_members current_membership
  WHERE current_membership.roleid = 'lifetime_member_group'::regrole
    AND current_membership.member = 'lifetime_member_first'::regrole;

REVOKE lifetime_member_group FROM lifetime_member_first;

GRANT lifetime_member_group TO lifetime_member_second;

SELECT saved.oid <> current_membership.oid
  FROM lifetime_saved_membership saved, pg_auth_members current_membership
  WHERE current_membership.roleid = 'lifetime_member_group'::regrole
    AND current_membership.member = 'lifetime_member_second'::regrole;

GRANT lifetime_member_group TO lifetime_member_first WITH ADMIN OPTION;

CREATE TABLE lifetime_saved_secondary_membership AS
  SELECT oid FROM pg_auth_members
  WHERE roleid = 'lifetime_member_group'::regrole
    AND member = 'lifetime_member_second'::regrole;

SET ROLE lifetime_member_first;

GRANT lifetime_member_group TO lifetime_member_second;

RESET ROLE;

SELECT count(*) = 2 FROM pg_auth_members
  WHERE roleid = 'lifetime_member_group'::regrole
    AND member = 'lifetime_member_second'::regrole;

SELECT count(*) = 1 FROM lifetime_saved_secondary_membership saved
  JOIN pg_auth_members current_membership ON current_membership.oid = saved.oid
  WHERE current_membership.grantor = 'postgres'::regrole;

SET ROLE lifetime_member_first;

REVOKE lifetime_member_group FROM lifetime_member_second;

RESET ROLE;

SELECT count(*) = 1 AND min(oid) = (SELECT oid FROM lifetime_saved_secondary_membership)
  FROM pg_auth_members
  WHERE roleid = 'lifetime_member_group'::regrole
    AND member = 'lifetime_member_second'::regrole;

GRANT lifetime_member_first TO lifetime_member_second;

SET ROLE lifetime_member_second;

GRANT lifetime_member_group TO lifetime_member_second;

RESET ROLE;

SELECT count(*) = 2 FROM pg_auth_members
  WHERE roleid = 'lifetime_member_group'::regrole
    AND member = 'lifetime_member_second'::regrole;

SELECT count(*) = 1 FROM pg_auth_members
  WHERE roleid = 'lifetime_member_group'::regrole
    AND member = 'lifetime_member_second'::regrole
    AND grantor = 'lifetime_member_first'::regrole;

SET ROLE lifetime_member_second;

REVOKE lifetime_member_group FROM lifetime_member_second;

RESET ROLE;

SELECT count(*) = 1 AND min(oid) = (SELECT oid FROM lifetime_saved_secondary_membership)
  FROM pg_auth_members
  WHERE roleid = 'lifetime_member_group'::regrole
    AND member = 'lifetime_member_second'::regrole;

RESET ROLE;

REVOKE lifetime_member_first FROM lifetime_member_second;

ALTER ROLE lifetime_member_second SUPERUSER;

SET ROLE lifetime_member_second;

GRANT lifetime_member_group TO lifetime_member_second;

SELECT count(*) = 1 AND min(oid) = (SELECT oid FROM lifetime_saved_secondary_membership)
  FROM pg_auth_members
  WHERE roleid = 'lifetime_member_group'::regrole
    AND member = 'lifetime_member_second'::regrole;

RESET ROLE;

ALTER ROLE lifetime_member_second NOSUPERUSER;

ALTER DEFAULT PRIVILEGES IN SCHEMA lifetime_privileges
  GRANT SELECT ON TABLES TO lifetime_member_first;

CREATE TABLE lifetime_saved_defaults AS
  SELECT oid FROM pg_default_acl
  WHERE defaclnamespace = 'lifetime_privileges'::regnamespace
    AND defaclobjtype = 'r';

ALTER DEFAULT PRIVILEGES IN SCHEMA lifetime_privileges
  GRANT INSERT ON TABLES TO lifetime_member_second;

SELECT saved.oid = current_defaults.oid
  FROM lifetime_saved_defaults saved, pg_default_acl current_defaults
  WHERE current_defaults.defaclnamespace = 'lifetime_privileges'::regnamespace
    AND current_defaults.defaclobjtype = 'r';

ALTER DEFAULT PRIVILEGES IN SCHEMA lifetime_privileges
  REVOKE SELECT ON TABLES FROM lifetime_member_first;

SELECT saved.oid = current_defaults.oid
  FROM lifetime_saved_defaults saved, pg_default_acl current_defaults
  WHERE current_defaults.defaclnamespace = 'lifetime_privileges'::regnamespace
    AND current_defaults.defaclobjtype = 'r';

ALTER DEFAULT PRIVILEGES IN SCHEMA lifetime_privileges
  REVOKE INSERT ON TABLES FROM lifetime_member_second;

SELECT count(*) = 0 FROM pg_default_acl
  WHERE defaclnamespace = 'lifetime_privileges'::regnamespace
    AND defaclobjtype = 'r';

ALTER DEFAULT PRIVILEGES IN SCHEMA lifetime_privileges
  GRANT SELECT ON TABLES TO lifetime_member_first;

SELECT saved.oid <> current_defaults.oid
  FROM lifetime_saved_defaults saved, pg_default_acl current_defaults
  WHERE current_defaults.defaclnamespace = 'lifetime_privileges'::regnamespace
    AND current_defaults.defaclobjtype = 'r';

ALTER DEFAULT PRIVILEGES REVOKE USAGE ON TYPES FROM PUBLIC;

SELECT cardinality(defaclacl) = 1 FROM pg_default_acl
  WHERE defaclrole = CURRENT_USER::regrole AND defaclnamespace = 0
    AND defaclobjtype = 'T';

ALTER DEFAULT PRIVILEGES GRANT USAGE ON TYPES TO PUBLIC;

SELECT count(*) = 0 FROM pg_default_acl
  WHERE defaclrole = CURRENT_USER::regrole AND defaclnamespace = 0
    AND defaclobjtype = 'T';

ALTER DEFAULT PRIVILEGES REVOKE ALL ON TABLES FROM CURRENT_USER;

SELECT cardinality(defaclacl) = 0 FROM pg_default_acl
  WHERE defaclrole = CURRENT_USER::regrole AND defaclnamespace = 0
    AND defaclobjtype = 'r';

ALTER DEFAULT PRIVILEGES GRANT ALL ON TABLES TO CURRENT_USER;

SELECT count(*) = 0 FROM pg_default_acl
  WHERE defaclrole = CURRENT_USER::regrole AND defaclnamespace = 0
    AND defaclobjtype = 'r';

DROP SCHEMA lifetime_privileges;

CREATE SCHEMA lifetime_privileges;

SELECT count(*) = 0 FROM pg_default_acl
  WHERE defaclrole = CURRENT_USER::regrole AND defaclobjtype = 'r';

ALTER DEFAULT PRIVILEGES IN SCHEMA lifetime_privileges
  GRANT SELECT ON TABLES TO lifetime_member_first;

CREATE TABLE lifetime_partition_root(id integer) PARTITION BY RANGE (id);

CREATE TABLE lifetime_partition_child PARTITION OF lifetime_partition_root
  FOR VALUES FROM (0) TO (10);

CREATE FUNCTION lifetime_partition_hook() RETURNS trigger LANGUAGE plpgsql
  AS 'BEGIN RETURN NEW; END';

CREATE TRIGGER lifetime_partition_hook AFTER INSERT ON lifetime_partition_root
  FOR EACH ROW EXECUTE FUNCTION lifetime_partition_hook();

CREATE TABLE lifetime_saved_partition_trigger AS
  SELECT oid FROM pg_trigger
  WHERE tgrelid = 'lifetime_partition_child'::regclass
    AND tgname = 'lifetime_partition_hook';

ALTER TABLE lifetime_partition_child DISABLE TRIGGER lifetime_partition_hook;

SELECT saved.oid = current_trigger.oid AND current_trigger.tgenabled = 'D'
  FROM lifetime_saved_partition_trigger saved, pg_trigger current_trigger
  WHERE current_trigger.tgrelid = 'lifetime_partition_child'::regclass
    AND current_trigger.tgname = 'lifetime_partition_hook';

DROP TABLE lifetime_partition_child;

CREATE TABLE lifetime_partition_child PARTITION OF lifetime_partition_root
  FOR VALUES FROM (0) TO (10);

SELECT saved.oid <> current_trigger.oid AND current_trigger.tgenabled = 'O'
  FROM lifetime_saved_partition_trigger saved, pg_trigger current_trigger
  WHERE current_trigger.tgrelid = 'lifetime_partition_child'::regclass
    AND current_trigger.tgname = 'lifetime_partition_hook';

CREATE TABLE lifetime_saved_reattachment AS
  SELECT oid FROM pg_trigger
  WHERE tgrelid = 'lifetime_partition_child'::regclass
    AND tgname = 'lifetime_partition_hook';

ALTER TABLE lifetime_partition_root DETACH PARTITION lifetime_partition_child;

SELECT count(*) = 0 FROM pg_trigger
  WHERE tgrelid = 'lifetime_partition_child'::regclass
    AND tgname = 'lifetime_partition_hook';

ALTER TABLE lifetime_partition_root ATTACH PARTITION lifetime_partition_child
  FOR VALUES FROM (0) TO (10);

SELECT saved.oid <> current_trigger.oid
  FROM lifetime_saved_reattachment saved, pg_trigger current_trigger
  WHERE current_trigger.tgrelid = 'lifetime_partition_child'::regclass
    AND current_trigger.tgname = 'lifetime_partition_hook';

CREATE FUNCTION lifetime_query(value integer) RETURNS integer LANGUAGE sql
  AS 'SELECT $1';

CREATE POLICY lifetime_row_policy ON lifetime_saved_defaults USING (true);

CREATE RULE lifetime_rewrite AS ON UPDATE TO lifetime_saved_defaults
  DO INSTEAD NOTHING;

CREATE TABLE lifetime_saved_definitions AS
  SELECT 'function'::text AS kind, 'lifetime_query(integer)'::regprocedure::oid AS oid
  UNION ALL SELECT 'policy'::text, oid FROM pg_policy WHERE polname = 'lifetime_row_policy'
  UNION ALL SELECT 'rule'::text, oid FROM pg_rewrite WHERE rulename = 'lifetime_rewrite';

CREATE OR REPLACE FUNCTION lifetime_query(value integer) RETURNS integer LANGUAGE sql
  AS 'SELECT $1 + 1';

ALTER POLICY lifetime_row_policy ON lifetime_saved_defaults USING (false);

ALTER TABLE lifetime_saved_defaults DISABLE RULE lifetime_rewrite;

SELECT saved.oid = 'lifetime_query(integer)'::regprocedure::oid
  FROM lifetime_saved_definitions saved
  WHERE saved.kind = 'function';

SELECT saved.oid = current_definition.oid
  FROM lifetime_saved_definitions saved, pg_policy current_definition
  WHERE saved.kind = 'policy' AND current_definition.polname = 'lifetime_row_policy';

SELECT saved.oid = current_definition.oid
  FROM lifetime_saved_definitions saved, pg_rewrite current_definition
  WHERE saved.kind = 'rule' AND current_definition.rulename = 'lifetime_rewrite';

DROP FUNCTION lifetime_query(integer);

CREATE FUNCTION lifetime_query(value integer) RETURNS integer LANGUAGE sql
  AS 'SELECT $1 + 2';

DROP POLICY lifetime_row_policy ON lifetime_saved_defaults;

CREATE POLICY lifetime_row_policy ON lifetime_saved_defaults USING (true);

DROP RULE lifetime_rewrite ON lifetime_saved_defaults;

CREATE RULE lifetime_rewrite AS ON UPDATE TO lifetime_saved_defaults
  DO INSTEAD NOTHING;

SELECT saved.oid <> 'lifetime_query(integer)'::regprocedure::oid
  FROM lifetime_saved_definitions saved
  WHERE saved.kind = 'function';

SELECT saved.oid <> current_definition.oid
  FROM lifetime_saved_definitions saved, pg_policy current_definition
  WHERE saved.kind = 'policy' AND current_definition.polname = 'lifetime_row_policy';

SELECT saved.oid <> current_definition.oid
  FROM lifetime_saved_definitions saved, pg_rewrite current_definition
  WHERE saved.kind = 'rule' AND current_definition.rulename = 'lifetime_rewrite';

-- cleanup

DROP TABLE lifetime_partition_root CASCADE;

DROP FUNCTION lifetime_partition_hook();

DROP FUNCTION lifetime_query(integer);

DROP TABLE lifetime_saved_membership, lifetime_saved_secondary_membership, lifetime_saved_defaults,
  lifetime_saved_partition_trigger, lifetime_saved_reattachment, lifetime_saved_definitions;

ALTER DEFAULT PRIVILEGES IN SCHEMA lifetime_privileges
  REVOKE SELECT ON TABLES FROM lifetime_member_first;

DROP SCHEMA lifetime_privileges;

DROP ROLE lifetime_member_first, lifetime_member_second, lifetime_member_group;
