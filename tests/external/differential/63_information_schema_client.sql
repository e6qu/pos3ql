-- Information-schema interfaces used by schema browsers and migration tools
-- must project the same typed role, ACL, and collation state as pg_catalog.
DROP TABLE IF EXISTS information_schema_client_privileges;
DROP ROLE IF EXISTS information_schema_client_reader;
DROP ROLE IF EXISTS information_schema_client_member;
DROP ROLE IF EXISTS information_schema_client_parent;

CREATE ROLE information_schema_client_parent;
CREATE ROLE information_schema_client_member;
GRANT information_schema_client_parent TO information_schema_client_member WITH ADMIN OPTION;
CREATE ROLE information_schema_client_reader;
CREATE TABLE information_schema_client_privileges (first integer, second text);
GRANT SELECT ON information_schema_client_privileges TO information_schema_client_reader;

SELECT collation_name, pad_attribute
  FROM information_schema.collations
 WHERE collation_name IN ('C', 'POSIX', 'ucs_basic')
 ORDER BY collation_name;
SELECT collation_name, character_set_name
  FROM information_schema.collation_character_set_applicability
 WHERE collation_name = 'C';
SELECT grantee, role_name, is_grantable
  FROM information_schema.applicable_roles
 WHERE role_name = 'information_schema_client_parent';
SELECT grantee, role_name
  FROM information_schema.administrable_role_authorizations
 WHERE role_name = 'information_schema_client_parent';
SELECT role_name
  FROM information_schema.enabled_roles
 WHERE role_name IN ('information_schema_client_member', 'information_schema_client_parent')
 ORDER BY role_name;
SELECT grantee, column_name, privilege_type, is_grantable
  FROM information_schema.column_privileges
 WHERE table_name = 'information_schema_client_privileges'
   AND grantee = 'information_schema_client_reader'
 ORDER BY column_name, privilege_type;
SELECT grantee, column_name, privilege_type
  FROM information_schema.role_column_grants
 WHERE table_name = 'information_schema_client_privileges'
   AND grantee = 'information_schema_client_reader'
 ORDER BY column_name, privilege_type;

DROP TABLE information_schema_client_privileges;
DROP ROLE information_schema_client_reader;
DROP ROLE information_schema_client_member;
DROP ROLE information_schema_client_parent;
