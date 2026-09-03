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
