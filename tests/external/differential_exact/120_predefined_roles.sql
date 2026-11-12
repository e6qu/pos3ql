-- PostgreSQL's predefined roles exist from initdb, keep their names and
-- OIDs, and cannot be created, altered, renamed or dropped.
SELECT oid, rolname, rolsuper, rolinherit, rolcreaterole, rolcreatedb,
       rolcanlogin, rolreplication, rolbypassrls, rolconnlimit
  FROM pg_authid WHERE oid < 16384 ORDER BY oid;
SELECT rolname FROM pg_roles WHERE oid < 16384 AND oid <> 10 ORDER BY rolname;
SELECT oid, roleid::regrole, member::regrole, grantor::regrole,
       admin_option, inherit_option, set_option
  FROM pg_auth_members WHERE member = 'pg_monitor'::regrole ORDER BY oid;
SELECT 'pg_monitor'::regrole::oid, 'pg_signal_backend'::regrole, 4544::regrole;
SELECT pg_has_role('postgres', 'pg_database_owner', 'MEMBER'),
       pg_has_role('pg_monitor', 'pg_read_all_stats', 'USAGE');
CREATE ROLE pg_custom;
CREATE ROLE pr_admin CREATEROLE;
CREATE ROLE pr_user;
CREATE ROLE pr_other;
ALTER ROLE pg_monitor LOGIN;
ALTER ROLE pg_missing CONNECTION LIMIT 3;
ALTER ROLE pg_monitor SET work_mem = '1MB';
ALTER ROLE pg_monitor RENAME TO monitor;
ALTER ROLE pr_user RENAME TO pg_user_renamed;
ALTER ROLE pr_user RENAME TO pr_other;
DROP ROLE pg_monitor;
DROP ROLE IF EXISTS pg_database_owner;
DROP ROLE postgres;
GRANT pg_database_owner TO pr_user;
GRANT pr_user TO pg_database_owner;
CREATE ROLE pr_bad IN ROLE pg_database_owner;
CREATE ROLE pr_bad ROLE pg_database_owner;
GRANT pg_monitor TO pr_user;
GRANT pg_read_all_settings TO pg_monitor;
GRANT pg_monitor TO pg_read_all_stats;
SET ROLE pr_admin;
GRANT pg_signal_backend TO pr_user;
GRANT pr_user TO pr_other;
ALTER ROLE pr_user RENAME TO pr_renamed;
DROP ROLE pr_other;
DROP ROLE postgres;
RESET ROLE;
COMMENT ON ROLE pg_monitor IS 'monitoring';
SELECT shobj_description('pg_monitor'::regrole, 'pg_authid');
COMMENT ON ROLE pg_monitor IS NULL;
-- Revoking an initdb membership removes it; granting it again creates a new row.
BEGIN;
REVOKE pg_read_all_stats FROM pg_monitor;
SELECT roleid::regrole FROM pg_auth_members WHERE member = 'pg_monitor'::regrole ORDER BY 1;
SELECT pg_has_role('pg_monitor', 'pg_read_all_stats', 'MEMBER');
ROLLBACK;
SELECT pg_has_role('pr_user', 'pg_read_all_stats', 'USAGE'),
       pg_has_role('pr_user', 'pg_monitor', 'MEMBER');
-- Predefined roles grant privileges whatever an object's ACL says.
CREATE SCHEMA pr_schema;
CREATE TABLE pr_schema.pr_t(a integer);
INSERT INTO pr_schema.pr_t VALUES (1);
CREATE SEQUENCE pr_schema.pr_seq;
SET ROLE pr_other;
SELECT * FROM pr_schema.pr_t;
RESET ROLE;
GRANT pg_read_all_data TO pr_other;
SET ROLE pr_other;
SELECT * FROM pr_schema.pr_t;
SELECT last_value FROM pr_schema.pr_seq;
INSERT INTO pr_schema.pr_t VALUES (2);
SELECT has_table_privilege('pr_schema.pr_t', 'SELECT'),
       has_table_privilege('pr_schema.pr_t', 'INSERT'),
       has_schema_privilege('pr_schema', 'USAGE'),
       has_schema_privilege('pr_schema', 'CREATE');
RESET ROLE;
GRANT pg_write_all_data TO pr_other;
SET ROLE pr_other;
INSERT INTO pr_schema.pr_t VALUES (2);
UPDATE pr_schema.pr_t SET a = a + 10;
DELETE FROM pr_schema.pr_t WHERE a = 12;
TRUNCATE pr_schema.pr_t;
ANALYZE pr_schema.pr_t;
RESET ROLE;
GRANT pg_maintain TO pr_other;
SET ROLE pr_other;
ANALYZE pr_schema.pr_t;
VACUUM pr_schema.pr_t;
SELECT has_table_privilege('pr_schema.pr_t', 'MAINTAIN');
CHECKPOINT;
RESET ROLE;
GRANT pg_checkpoint TO pr_other;
SET ROLE pr_other;
CHECKPOINT;
RESET ROLE;
REVOKE pg_read_all_data, pg_write_all_data, pg_maintain, pg_checkpoint FROM pr_other;
SET ROLE pr_other;
SELECT * FROM pr_schema.pr_t;
SELECT pg_cancel_backend(pg_backend_pid());
RESET ROLE;
-- initdb's EXECUTE grants to predefined roles.
SELECT p.oid::regprocedure, p.proacl FROM pg_proc p
 WHERE p.proacl::text LIKE '%pg_%' ORDER BY p.oid;
SELECT objoid::regprocedure, initprivs FROM pg_init_privs
 WHERE classoid = 'pg_proc'::regclass AND initprivs::text LIKE '%pg_%' ORDER BY objoid;
SELECT has_function_privilege('pr_other', 'pg_ls_waldir()', 'EXECUTE'),
       has_function_privilege('pg_monitor', 'pg_ls_waldir()', 'EXECUTE'),
       has_function_privilege('pg_read_all_stats', 'pg_get_backend_memory_contexts()', 'EXECUTE');
GRANT pg_monitor TO pr_other;
SELECT has_function_privilege('pr_other', 'pg_ls_waldir()', 'EXECUTE');
GRANT EXECUTE ON FUNCTION pg_ls_waldir() TO pr_user;
SELECT proacl FROM pg_proc WHERE oid = 'pg_ls_waldir()'::regprocedure;
REVOKE EXECUTE ON FUNCTION pg_ls_waldir() FROM pr_user;
SELECT proacl FROM pg_proc WHERE oid = 'pg_ls_waldir()'::regprocedure;
DROP SCHEMA pr_schema CASCADE;
DROP ROLE pr_user, pr_other, pr_admin;
-- MAINTAIN: VACUUM and ANALYZE skip tables the role may not maintain.
CREATE TABLE pr_owned(a integer);
CREATE ROLE pr_maint;
GRANT CREATE ON SCHEMA public TO pr_maint;
SET ROLE pr_maint;
CREATE TABLE pr_mine(a integer);
ANALYZE pr_owned, pr_mine;
VACUUM pr_owned, pr_mine;
VACUUM ANALYZE pr_owned;
ANALYZE pr_owned, pr_missing;
REINDEX TABLE pr_owned;
BEGIN;
LOCK TABLE pr_owned;
ROLLBACK;
RESET ROLE;
GRANT pg_maintain TO pr_maint;
SET ROLE pr_maint;
ANALYZE pr_owned;
VACUUM pr_owned;
BEGIN;
LOCK TABLE pr_owned;
ROLLBACK;
RESET ROLE;
DROP TABLE pr_owned, pr_mine;
REVOKE CREATE ON SCHEMA public FROM pr_maint;
DROP ROLE pr_maint;
