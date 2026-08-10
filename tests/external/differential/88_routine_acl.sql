CREATE ROLE routine_acl_owner;
CREATE ROLE routine_acl_reader;
CREATE ROLE routine_acl_other;
GRANT CREATE ON SCHEMA public TO routine_acl_owner;

SET ROLE routine_acl_owner;
ALTER DEFAULT PRIVILEGES REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
ALTER DEFAULT PRIVILEGES GRANT EXECUTE ON FUNCTIONS TO routine_acl_reader;
CREATE FUNCTION routine_acl_answer() RETURNS integer LANGUAGE SQL AS 'SELECT 42';
CREATE FUNCTION routine_acl_answer(integer) RETURNS integer LANGUAGE SQL AS 'SELECT $1 + 1';
RESET ROLE;

SELECT has_function_privilege('routine_acl_reader', 'routine_acl_answer()', 'EXECUTE'),
       has_function_privilege('routine_acl_other', 'routine_acl_answer()', 'EXECUTE');
GRANT EXECUTE ON FUNCTION routine_acl_answer() TO routine_acl_other;
SELECT has_function_privilege('routine_acl_other', 'routine_acl_answer()', 'EXECUTE');
SELECT grantor, grantee, routine_name, privilege_type, is_grantable
  FROM information_schema.routine_privileges
 WHERE routine_name = 'routine_acl_answer' AND grantee = 'routine_acl_other';
SET ROLE routine_acl_other;
SELECT routine_acl_answer();
RESET ROLE;

SET ROLE routine_acl_owner;
CREATE OR REPLACE FUNCTION routine_acl_answer() RETURNS integer LANGUAGE SQL AS 'SELECT 43';
RESET ROLE;
SET ROLE routine_acl_other;
SELECT routine_acl_answer();
RESET ROLE;

REVOKE EXECUTE ON ALL FUNCTIONS IN SCHEMA public FROM routine_acl_reader;
SELECT has_function_privilege('routine_acl_reader', 'routine_acl_answer()', 'EXECUTE'),
       has_function_privilege('routine_acl_reader', 'routine_acl_answer(integer)', 'EXECUTE');

DROP FUNCTION routine_acl_answer(integer);
DROP FUNCTION routine_acl_answer();
REVOKE CREATE ON SCHEMA public FROM routine_acl_owner;
DROP ROLE routine_acl_other;
DROP ROLE routine_acl_reader;
DROP ROLE routine_acl_owner;
