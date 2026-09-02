-- PostgreSQL 18.4 role, membership, ownership, and object-privilege surface.
-- Names are isolated and every object is removed so later corpora start clean.

CREATE ROLE acl_owner;
CREATE ROLE acl_reader;
CREATE ROLE acl_administrator CREATEROLE;
CREATE ROLE acl_managed;
COMMENT ON ROLE acl_owner IS 'acl owner comment';
SELECT shobj_description(oid, 'pg_authid')
  FROM pg_roles WHERE rolname = 'acl_owner';
GRANT acl_managed TO acl_administrator WITH ADMIN OPTION;
GRANT CREATE ON SCHEMA public TO acl_owner;
CREATE ROLE parameter_actor;
GRANT SET ON PARAMETER event_triggers TO parameter_actor;
SELECT has_parameter_privilege('parameter_actor', 'event_triggers', 'SET'),
       has_parameter_privilege('parameter_actor', 'event_triggers', 'ALTER SYSTEM');
SELECT 'pg_parameter_acl'::regclass::oid;
SELECT parname, paracl::text FROM pg_parameter_acl;
SET ROLE parameter_actor;
SET event_triggers = off;
RESET ROLE;
REVOKE SET ON PARAMETER event_triggers FROM parameter_actor;
DROP ROLE parameter_actor;

SELECT acldefault('F', oid)::text, acldefault('L', oid)::text,
       acldefault('S', oid)::text, acldefault('T', oid)::text,
       acldefault('c', oid)::text, acldefault('d', oid)::text,
       acldefault('f', oid)::text, acldefault('l', oid)::text,
       acldefault('n', oid)::text, acldefault('p', oid)::text,
       acldefault('r', oid)::text, acldefault('s', oid)::text,
       acldefault('t', oid)::text
  FROM pg_roles WHERE rolname = 'postgres';

SET ROLE acl_owner;
CREATE TABLE acl_private (id integer PRIMARY KEY, value text);
INSERT INTO acl_private VALUES (1, 'visible-through-owner');
CREATE VIEW acl_exposed AS SELECT value FROM acl_private;
CREATE SEQUENCE acl_sequence;
CREATE TYPE acl_state AS ENUM ('ready', 'blocked');
RESET ROLE;

GRANT SELECT ON acl_exposed TO acl_reader GRANTED BY CURRENT_USER;
GRANT USAGE ON SEQUENCE acl_sequence TO acl_reader;
SELECT grantor, grantee, table_name, privilege_type, is_grantable, with_hierarchy
FROM information_schema.table_privileges
WHERE table_name = 'acl_exposed'
ORDER BY grantor, grantee, privilege_type;
SELECT grantor, grantee, table_name, privilege_type, is_grantable, with_hierarchy
FROM information_schema.role_table_grants
WHERE table_name = 'acl_exposed'
ORDER BY grantor, grantee, privilege_type;
SELECT data_type, numeric_precision, numeric_precision_radix, numeric_scale,
       start_value, minimum_value, maximum_value, increment, cycle_option
FROM information_schema.sequences
WHERE sequence_name = 'acl_sequence';
SELECT grantor, grantee, object_type, object_name, privilege_type, is_grantable
FROM information_schema.usage_privileges
WHERE object_name = 'acl_sequence'
ORDER BY grantor, grantee;
SELECT has_table_privilege('acl_reader', 'acl_exposed', 'SELECT'),
       has_table_privilege('acl_reader', 'acl_private', 'SELECT'),
       has_sequence_privilege('acl_reader', 'acl_sequence', 'USAGE'),
       has_schema_privilege('acl_reader', 'public', 'USAGE'),
       has_type_privilege('acl_reader', 'acl_state', 'USAGE');

SET ROLE acl_reader;
SELECT value FROM acl_exposed;
SELECT nextval('acl_sequence');
RESET ROLE;

ALTER ROLE acl_owner RENAME TO acl_renamed_owner;
SELECT pg_get_userbyid(c.relowner)
  FROM pg_class c
 WHERE c.relname = 'acl_private';
SELECT shobj_description(oid, 'pg_authid')
  FROM pg_roles WHERE rolname = 'acl_renamed_owner';

DROP VIEW acl_exposed;
DROP TABLE acl_private;
DROP SEQUENCE acl_sequence;
DROP TYPE acl_state;
REVOKE acl_managed FROM acl_administrator;
REVOKE CREATE ON SCHEMA public FROM acl_renamed_owner GRANTED BY CURRENT_USER;
DROP ROLE acl_reader;
DROP ROLE acl_renamed_owner;
DROP ROLE acl_managed;
DROP ROLE acl_administrator;

CREATE ROLE owned_source;
CREATE ROLE owned_target;
GRANT CREATE ON SCHEMA public TO owned_source;
ALTER DEFAULT PRIVILEGES FOR ROLE owned_source
  GRANT SELECT ON TABLES TO owned_target WITH GRANT OPTION;
ALTER DEFAULT PRIVILEGES FOR ROLE owned_source IN SCHEMA public
  GRANT INSERT ON TABLES TO owned_target;
ALTER DEFAULT PRIVILEGES FOR ROLE owned_source
  REVOKE USAGE ON TYPES FROM PUBLIC;

SET SESSION AUTHORIZATION owned_source;
SELECT session_user, current_user, current_role;
RESET SESSION AUTHORIZATION;

SET ROLE owned_source;
CREATE TABLE owned_default_table (id integer);
CREATE TYPE owned_default_type AS ENUM ('ready');
INSERT INTO owned_default_table VALUES (1);
RESET ROLE;

SET ROLE owned_target;
INSERT INTO owned_default_table VALUES (2);
SELECT id FROM owned_default_table ORDER BY id;
RESET ROLE;
SELECT has_type_privilege('owned_target', 'owned_default_type', 'USAGE');

REASSIGN OWNED BY owned_source TO owned_target;
SELECT tableowner FROM pg_tables WHERE tablename = 'owned_default_table';
SELECT relacl::text FROM pg_class WHERE relname = 'owned_default_table';
SELECT typacl::text FROM pg_type WHERE typname = 'owned_default_type';
DROP OWNED BY owned_source;
DROP ROLE owned_source;
DROP OWNED BY owned_target CASCADE;
DROP ROLE owned_target;

CREATE ROLE membership_parent;
CREATE ROLE membership_child NOINHERIT;
CREATE ROLE membership_leaf;
GRANT membership_parent TO membership_child;
GRANT membership_parent TO membership_child WITH SET FALSE;
GRANT membership_parent TO membership_child WITH ADMIN TRUE;
SET ROLE membership_child;
GRANT membership_parent TO membership_leaf;
RESET ROLE;
SELECT membership.oid > 0, member.rolname, grantor.rolname,
       membership.admin_option, membership.inherit_option, membership.set_option
  FROM pg_auth_members membership
  JOIN pg_roles member ON member.oid = membership.member
  JOIN pg_roles grantor ON grantor.oid = membership.grantor
 WHERE membership.roleid = (SELECT oid FROM pg_roles
                             WHERE rolname = 'membership_parent')
 ORDER BY member.rolname;
REVOKE ADMIN OPTION FOR membership_parent FROM membership_child CASCADE;
SELECT member.rolname, membership.admin_option, membership.inherit_option,
       membership.set_option
  FROM pg_auth_members membership
  JOIN pg_roles member ON member.oid = membership.member
 WHERE membership.roleid = (SELECT oid FROM pg_roles
                             WHERE rolname = 'membership_parent');

ALTER ROLE ALL IN DATABASE postgres SET application_name TO 'database-default';
ALTER ROLE membership_child SET application_name TO 'role-default';
ALTER ROLE membership_child IN DATABASE postgres
  SET application_name TO 'role-database-default';
SELECT setdatabase <> 0, setrole <> 0, setconfig::text
  FROM pg_db_role_setting
 ORDER BY setdatabase, setrole;
ALTER ROLE membership_child IN DATABASE postgres RESET application_name;
ALTER ROLE membership_child RESET application_name;
ALTER ROLE ALL IN DATABASE postgres RESET application_name;
REVOKE membership_parent FROM membership_child;
DROP ROLE membership_leaf;
DROP ROLE membership_child;
DROP ROLE membership_parent;

CREATE ROLE database_actor;
REVOKE CONNECT, TEMPORARY ON DATABASE postgres FROM PUBLIC;
GRANT CONNECT, CREATE ON DATABASE postgres TO database_actor WITH GRANT OPTION;
SELECT datacl::text FROM pg_database WHERE datname = 'postgres';
SELECT has_database_privilege('database_actor', 'postgres', 'CONNECT, CREATE'),
       has_database_privilege('database_actor', 5::oid, 'TEMPORARY'),
       has_database_privilege('database_actor', 'postgres', 'CREATE WITH GRANT OPTION');
SET ROLE database_actor;
CREATE SCHEMA database_actor_schema;
DROP SCHEMA database_actor_schema;
RESET ROLE;
REVOKE GRANT OPTION FOR CREATE ON DATABASE postgres FROM database_actor;
REVOKE CONNECT, CREATE ON DATABASE postgres FROM database_actor;
GRANT CONNECT, TEMPORARY ON DATABASE postgres TO PUBLIC;
DROP ROLE database_actor;

CREATE ROLE column_actor;
CREATE ROLE column_delegate;
CREATE ROLE column_leaf;
GRANT CREATE ON SCHEMA public TO column_actor;
CREATE TABLE column_privilege_target (
  id integer PRIMARY KEY,
  visible text,
  mutable text,
  secret text
);
INSERT INTO column_privilege_target VALUES (1, 'shown', 'old', 'hidden');
GRANT SELECT (id, visible), INSERT (id, visible, mutable),
      UPDATE (mutable), REFERENCES (id)
  ON column_privilege_target TO column_actor;
GRANT SELECT (visible) ON column_privilege_target TO column_delegate
  WITH GRANT OPTION;
SELECT has_column_privilege('column_actor', 'column_privilege_target', 'visible', 'SELECT'),
       has_column_privilege('column_actor', 'column_privilege_target', 4::smallint, 'SELECT'),
       has_column_privilege('column_actor', 'column_privilege_target', 'mutable', 'UPDATE'),
       has_any_column_privilege('column_actor', 'column_privilege_target', 'SELECT');
SELECT attname, attacl::text
  FROM pg_attribute
 WHERE attrelid = 'column_privilege_target'::regclass
   AND attname IN ('visible', 'mutable')
 ORDER BY attname;
SELECT grantee, column_name, privilege_type, is_grantable
  FROM information_schema.column_privileges
 WHERE table_name = 'column_privilege_target'
   AND grantee IN ('column_actor', 'column_delegate')
 ORDER BY grantee, column_name, privilege_type;
SET ROLE column_actor;
SELECT visible FROM column_privilege_target;
INSERT INTO column_privilege_target (id, visible, mutable)
  VALUES (2, 'second', 'new');
UPDATE column_privilege_target SET mutable = 'changed' WHERE id = 2;
CREATE TABLE column_privilege_child (
  parent_id integer REFERENCES column_privilege_target(id)
);
RESET ROLE;
SET ROLE column_delegate;
GRANT SELECT (visible) ON column_privilege_target TO column_leaf;
RESET ROLE;
REVOKE GRANT OPTION FOR SELECT (visible)
  ON column_privilege_target FROM column_delegate CASCADE;
SELECT has_column_privilege('column_leaf', 'column_privilege_target', 'visible', 'SELECT');
CREATE VIEW column_privilege_view AS
  SELECT visible, secret FROM column_privilege_target;
GRANT SELECT (visible) ON column_privilege_view TO column_actor;
SELECT has_column_privilege('column_actor', 'column_privilege_view', 'visible', 'SELECT'),
       has_column_privilege('column_actor', 'column_privilege_view', 'secret', 'SELECT');
SET ROLE column_actor;
SELECT visible FROM column_privilege_view ORDER BY visible;
RESET ROLE;
CREATE ROLE view_column_writer;
CREATE TABLE view_column_write_base (id integer, hidden text);
CREATE VIEW view_column_write AS SELECT id FROM view_column_write_base;
GRANT INSERT (id), UPDATE (id), SELECT (id) ON view_column_write TO view_column_writer;
GRANT DELETE ON view_column_write TO view_column_writer;
SET ROLE view_column_writer;
INSERT INTO view_column_write (id) VALUES (1) RETURNING id;
UPDATE view_column_write SET id = 2 WHERE id = 1 RETURNING id;
DELETE FROM view_column_write WHERE id = 2;
RESET ROLE;
SELECT count(*) FROM view_column_write_base;
DROP VIEW view_column_write;
DROP TABLE view_column_write_base;
DROP ROLE view_column_writer;
DROP VIEW column_privilege_view;
DROP TABLE column_privilege_child;
DROP TABLE column_privilege_target;
DROP ROLE column_leaf;
DROP ROLE column_delegate;
DROP ROLE column_actor;
