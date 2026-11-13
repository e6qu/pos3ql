-- Storing or applying a superuser-only parameter needs the privilege to set
-- it, as PostgreSQL checks: routine SET clauses at CREATE and ALTER and at
-- every call (with the owner's rights under SECURITY DEFINER), ALTER ROLE and
-- ALTER DATABASE settings, set_config and ALTER SYSTEM.
CREATE SCHEMA parameter_set_privileges;
SET search_path = parameter_set_privileges, public;
CREATE ROLE parameter_set_user;
CREATE ROLE parameter_set_admin CREATEROLE;
GRANT USAGE, CREATE ON SCHEMA parameter_set_privileges TO parameter_set_user;
CREATE FUNCTION invoker_replica() RETURNS text LANGUAGE sql
  SET session_replication_role = replica
  AS $$ SELECT current_setting('session_replication_role') $$;
CREATE FUNCTION definer_replica() RETURNS text LANGUAGE sql SECURITY DEFINER
  SET session_replication_role = replica
  AS $$ SELECT current_setting('session_replication_role') $$;
CREATE PROCEDURE invoker_procedure() LANGUAGE plpgsql
  SET track_functions = 'all'
  AS $$ BEGIN RAISE NOTICE 'track_functions %', current_setting('track_functions'); END $$;
SET ROLE parameter_set_user;
SELECT invoker_replica();
SELECT definer_replica();
CALL invoker_procedure();
DO $$ BEGIN PERFORM invoker_replica(); END $$;
SELECT * FROM invoker_replica();
CREATE FUNCTION user_replica() RETURNS int LANGUAGE sql
  SET session_replication_role = replica AS $$ SELECT 1 $$;
CREATE FUNCTION user_replica() RETURNS int LANGUAGE sql
  SET session_replication_role = 'not a role' AS $$ SELECT 1 $$;
CREATE FUNCTION user_replica() RETURNS int LANGUAGE sql
  SET track_functions FROM CURRENT AS $$ SELECT 1 $$;
CREATE PROCEDURE user_procedure() LANGUAGE sql
  SET session_replication_role TO DEFAULT AS $$ SELECT 1 $$;
CREATE FUNCTION user_work_mem() RETURNS int LANGUAGE sql
  SET work_mem = '1MB' AS $$ SELECT 1 $$;
ALTER FUNCTION user_work_mem() SET session_replication_role = replica;
ALTER FUNCTION user_work_mem() SET session_replication_role FROM CURRENT;
ALTER FUNCTION user_work_mem() RESET session_replication_role;
ALTER FUNCTION user_work_mem() SET work_mem = '2MB';
ALTER FUNCTION user_work_mem() RESET ALL;
SELECT proconfig FROM pg_proc WHERE proname = 'user_work_mem';
ALTER ROLE parameter_set_user SET session_replication_role = replica;
ALTER ROLE parameter_set_user SET work_mem = '2MB';
ALTER ROLE parameter_set_user RESET session_replication_role;
ALTER ROLE parameter_set_user SET track_functions FROM CURRENT;
ALTER ROLE parameter_set_user IN DATABASE postgres SET track_functions = 'pl';
SET session_replication_role = replica;
SELECT set_config('session_replication_role', 'replica', false);
ALTER SYSTEM SET work_mem = '1MB';
ALTER SYSTEM RESET work_mem;
ALTER SYSTEM RESET ALL;
RESET ROLE;
GRANT SET ON PARAMETER session_replication_role TO parameter_set_user;
SET ROLE parameter_set_user;
SELECT invoker_replica();
CREATE FUNCTION user_replica() RETURNS text LANGUAGE sql
  SET session_replication_role = replica
  AS $$ SELECT current_setting('session_replication_role') $$;
SELECT user_replica();
ALTER FUNCTION user_work_mem() SET session_replication_role = local;
ALTER ROLE parameter_set_user SET session_replication_role = replica;
RESET ROLE;
REVOKE SET ON PARAMETER session_replication_role FROM parameter_set_user;
SET ROLE parameter_set_user;
-- The privilege is checked again at each call.
SELECT user_replica();
SELECT user_work_mem();
RESET ROLE;
SELECT user_replica();
SELECT setconfig FROM pg_db_role_setting
  WHERE setrole = 'parameter_set_user'::regrole;
SET ROLE parameter_set_admin;
CREATE ROLE parameter_set_child;
ALTER ROLE parameter_set_child SET session_replication_role = replica;
ALTER ROLE parameter_set_child SET work_mem = '3MB';
RESET ROLE;
SELECT setconfig FROM pg_db_role_setting
  WHERE setrole = 'parameter_set_child'::regrole;
ALTER ROLE parameter_set_child RESET ALL;
ALTER ROLE parameter_set_user RESET ALL;
DROP ROLE parameter_set_child;
DROP SCHEMA parameter_set_privileges CASCADE;
DROP OWNED BY parameter_set_user;
DROP ROLE parameter_set_user;
DROP ROLE parameter_set_admin;
