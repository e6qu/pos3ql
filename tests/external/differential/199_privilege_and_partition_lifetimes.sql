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

-- cleanup

DROP TABLE lifetime_partition_root CASCADE;

DROP FUNCTION lifetime_partition_hook();

DROP TABLE lifetime_saved_membership, lifetime_saved_defaults,
  lifetime_saved_partition_trigger, lifetime_saved_reattachment;

ALTER DEFAULT PRIVILEGES IN SCHEMA lifetime_privileges
  REVOKE SELECT ON TABLES FROM lifetime_member_first;

DROP SCHEMA lifetime_privileges;

DROP ROLE lifetime_member_first, lifetime_member_second, lifetime_member_group;
