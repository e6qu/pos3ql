-- PostgreSQL 18.4 role, membership, ownership, and object-privilege surface.
-- Names are isolated and every object is removed so later corpora start clean.

CREATE ROLE acl_owner;
CREATE ROLE acl_reader;
CREATE ROLE acl_administrator CREATEROLE;
CREATE ROLE acl_managed;
GRANT acl_managed TO acl_administrator WITH ADMIN OPTION;
GRANT CREATE ON SCHEMA public TO acl_owner;

SET ROLE acl_owner;
CREATE TABLE acl_private (id integer PRIMARY KEY, value text);
INSERT INTO acl_private VALUES (1, 'visible-through-owner');
CREATE VIEW acl_exposed AS SELECT value FROM acl_private;
CREATE SEQUENCE acl_sequence;
CREATE TYPE acl_state AS ENUM ('ready', 'blocked');
RESET ROLE;

GRANT SELECT ON acl_exposed TO acl_reader;
GRANT USAGE ON SEQUENCE acl_sequence TO acl_reader;
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

DROP VIEW acl_exposed;
DROP TABLE acl_private;
DROP SEQUENCE acl_sequence;
DROP TYPE acl_state;
REVOKE acl_managed FROM acl_administrator;
REVOKE CREATE ON SCHEMA public FROM acl_renamed_owner;
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
