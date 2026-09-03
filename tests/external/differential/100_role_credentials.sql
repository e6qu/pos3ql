-- PostgreSQL 18 stores an already-derived SCRAM verifier exactly, rather than
-- treating it as cleartext. ENCRYPTED is accepted syntax; UNENCRYPTED is not.

CREATE ROLE credential_import LOGIN ENCRYPTED PASSWORD
  'SCRAM-SHA-256$4096:rZAf+E/QiUOxIJMNkHvs7A==$9PemCa7bTdgkjy4cbv0qCKbvK+I3U7o168sYUHKkYR0=:5x1VtOZhM2IJVvOaA8sBH31DLM+uwunb7ioRy59bb6c=';
SELECT rolpassword =
  'SCRAM-SHA-256$4096:rZAf+E/QiUOxIJMNkHvs7A==$9PemCa7bTdgkjy4cbv0qCKbvK+I3U7o168sYUHKkYR0=:5x1VtOZhM2IJVvOaA8sBH31DLM+uwunb7ioRy59bb6c='
  FROM pg_authid WHERE rolname = 'credential_import';
ALTER ROLE credential_import PASSWORD NULL;
SELECT rolpassword IS NULL FROM pg_authid WHERE rolname = 'credential_import';
CREATE ROLE credential_plaintext PASSWORD 'SCRAM-SHA-256$4096:not-base64$bad:bad';
SELECT rolpassword LIKE 'SCRAM-SHA-256$%'
  FROM pg_authid WHERE rolname = 'credential_plaintext';
ALTER ROLE credential_import UNENCRYPTED PASSWORD 'not-supported';
DROP ROLE credential_plaintext;
DROP ROLE credential_import;

CREATE ROLE credential_sysid SYSID 7;
SELECT rolname FROM pg_roles WHERE rolname = 'credential_sysid';
DROP ROLE credential_sysid;

CREATE ROLE credential_default_expiry;
SELECT rolpassword = '********', rolvaliduntil IS NULL
  FROM pg_roles WHERE rolname = 'credential_default_expiry';
DROP ROLE credential_default_expiry;

CREATE ROLE credential_duplicate_option LOGIN NOLOGIN;
CREATE ROLE credential_duplicate_password PASSWORD 'one' ENCRYPTED PASSWORD 'two';
SELECT count(*) FROM pg_roles
WHERE rolname IN ('credential_duplicate_option',
                   'credential_duplicate_password');

CREATE ROLE credential_membership_parent;
CREATE ROLE credential_duplicate_membership
  IN ROLE credential_membership_parent IN GROUP credential_membership_parent;
SELECT count(*) FROM pg_roles WHERE rolname = 'credential_duplicate_membership';
CREATE ROLE credential_membership_member NOINHERIT;
CREATE ROLE credential_membership_administrator;
CREATE ROLE "session_user";
CREATE GROUP credential_membership_bundle
  IN GROUP credential_membership_parent, "session_user"
  USER credential_membership_member
  ADMIN credential_membership_administrator;
SELECT parent.rolname, member.rolname, membership.admin_option,
       membership.inherit_option, membership.set_option
  FROM pg_auth_members membership
  JOIN pg_roles parent ON parent.oid = membership.roleid
  JOIN pg_roles member ON member.oid = membership.member
 WHERE parent.rolname IN ('credential_membership_parent',
                          'session_user',
                          'credential_membership_bundle')
 ORDER BY parent.rolname, member.rolname;
DROP ROLE credential_membership_bundle;
DROP ROLE credential_membership_administrator;
DROP ROLE credential_membership_member;
DROP ROLE credential_membership_parent;
DROP ROLE "session_user";

CREATE USER credential_empty_password PASSWORD '';
SELECT rolpassword IS NULL
  FROM pg_authid WHERE rolname = 'credential_empty_password';
DROP USER credential_empty_password;

CREATE ROLE credential_md5 LOGIN
  PASSWORD 'md547fcb6615d41c53cd39822141eb05da2';
SELECT rolpassword = 'md547fcb6615d41c53cd39822141eb05da2'
  FROM pg_authid WHERE rolname = 'credential_md5';
ALTER ROLE credential_md5 RENAME TO credential_md5_renamed;
SELECT rolpassword IS NULL
  FROM pg_authid WHERE rolname = 'credential_md5_renamed';
DROP ROLE credential_md5_renamed;

SET password_encryption TO 'md5';
SHOW password_encryption;
CREATE ROLE credential_md5_generated LOGIN PASSWORD 'credential-secret';
SELECT rolpassword = 'md5a3221a0fd80db2ebd0ce7b454dd89139'
  FROM pg_authid WHERE rolname = 'credential_md5_generated';
ALTER ROLE credential_md5_generated PASSWORD 'replacement-secret';
SELECT rolpassword = 'md53c7622c401b4a25294783f1c91d31b4a'
  FROM pg_authid WHERE rolname = 'credential_md5_generated';
DROP ROLE credential_md5_generated;
RESET password_encryption;
SHOW password_encryption;

BEGIN;
SET LOCAL password_encryption TO 'md5';
CREATE ROLE credential_md5_local PASSWORD 'credential-secret';
COMMIT;
CREATE ROLE credential_scram_default PASSWORD 'credential-secret';
SELECT md5_role.rolpassword LIKE 'md5%',
       scram_role.rolpassword LIKE 'SCRAM-SHA-256$%'
  FROM pg_authid md5_role
  CROSS JOIN pg_authid scram_role
 WHERE md5_role.rolname = 'credential_md5_local'
   AND scram_role.rolname = 'credential_scram_default';
DROP ROLE credential_md5_local, credential_scram_default;

CREATE ROLE "current_user";
CREATE ROLE "all";
ALTER ROLE "current_user" NOLOGIN;
ALTER USER "all" LOGIN;
SELECT rolname, rolcanlogin
  FROM pg_roles
 WHERE rolname IN ('current_user', 'all')
 ORDER BY rolname;
ALTER ROLE CURRENT_ROLE SET application_name TO 'credential-current-role';
ALTER USER SESSION_USER RESET application_name;
DROP ROLE "current_user", "all";

CREATE ROLE credential_options
  NOSUPERUSER INHERIT CREATEROLE CREATEDB LOGIN NOREPLICATION NOBYPASSRLS
  CONNECTION LIMIT 4 VALID UNTIL 'infinity';
SELECT rolsuper, rolinherit, rolcreaterole, rolcreatedb, rolcanlogin,
       rolreplication, rolbypassrls, rolconnlimit, rolvaliduntil IS NULL
  FROM pg_roles WHERE rolname = 'credential_options';
ALTER ROLE credential_options
  NOINHERIT NOCREATEROLE NOCREATEDB NOLOGIN CONNECTION LIMIT -1
  PASSWORD NULL VALID UNTIL 'infinity';
SELECT rolinherit, rolcreaterole, rolcreatedb, rolcanlogin, rolconnlimit,
       rolpassword IS NULL, rolvaliduntil IS NULL
  FROM pg_roles WHERE rolname = 'credential_options';
DROP ROLE credential_options;

CREATE GROUP credential_group;
CREATE USER credential_user LOGIN;
ALTER GROUP credential_group ADD USER credential_user;
SELECT parent.rolname, member.rolname
  FROM pg_auth_members membership
  JOIN pg_roles parent ON parent.oid = membership.roleid
  JOIN pg_roles member ON member.oid = membership.member
 WHERE parent.rolname = 'credential_group';
ALTER GROUP credential_group DROP USER credential_user;
SELECT count(*)
  FROM pg_auth_members membership
  JOIN pg_roles parent ON parent.oid = membership.roleid
 WHERE parent.rolname = 'credential_group';
ALTER GROUP credential_group RENAME TO credential_group_renamed;
CREATE ROLE credential_bundle IN GROUP credential_group_renamed ROLE credential_user;
SELECT rolname, rolcanlogin
  FROM pg_roles
 WHERE rolname IN ('credential_bundle', 'credential_group_renamed', 'credential_user')
 ORDER BY rolname;
SELECT parent.rolname, member.rolname
  FROM pg_auth_members membership
  JOIN pg_roles parent ON parent.oid = membership.roleid
  JOIN pg_roles member ON member.oid = membership.member
 WHERE parent.rolname IN ('credential_bundle', 'credential_group_renamed')
 ORDER BY parent.rolname, member.rolname;
ALTER USER credential_user NOLOGIN;
SELECT rolcanlogin FROM pg_roles WHERE rolname = 'credential_user';
DROP ROLE credential_bundle;
DROP USER credential_user;
DROP GROUP credential_group_renamed;
