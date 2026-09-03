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
