-- Transactional run-time configuration, matching PostgreSQL 18: SET and
-- set_config(session) survive commit but not rollback; SET LOCAL and
-- set_config(local) last only through the transaction; savepoints restore both
-- the visible value and the session value that a later COMMIT will publish.

SET application_name = 'guc-baseline';
SET search_path = "$user", public;

-- set_config mutates during expression evaluation, so a later expression in
-- the same row sees the change.
SELECT set_config('application_name', 'from-function', false),
       current_setting('application_name');
SHOW application_name;

-- Rendering settings changed by set_config apply to the row that contains the
-- call, not only to the next statement.
SELECT set_config('bytea_output', 'escape', false), '\x4142'::bytea;
SELECT set_config('DateStyle', 'SQL, DMY', false), DATE '2024-01-02';
SELECT set_config('TimeZone', 'America/New_York', false),
       '2024-01-15 12:00:00+00'::timestamptz;
RESET bytea_output;
RESET DateStyle;
RESET TimeZone;

-- A NULL new value means RESET, while NULL is_local means session scope.
SELECT set_config('search_path', 'private', NULL),
       current_setting('search_path');
SELECT set_config('search_path', NULL, false),
       current_setting('search_path');

-- Session changes are transactional.
BEGIN;
SET application_name = 'doomed';
SELECT current_setting('application_name');
ROLLBACK;
SHOW application_name;

BEGIN;
SELECT set_config('application_name', 'committed', false);
COMMIT;
SHOW application_name;

-- A session assignment after an unrelated local overlay must not promote the
-- local value when the transaction commits.
BEGIN;
SET LOCAL search_path = private;
SET application_name = 'mixed-session';
COMMIT;
SELECT current_setting('application_name'), current_setting('search_path');

-- SET followed by SET LOCAL exposes the local value until transaction end,
-- then publishes the session value.
BEGIN;
SET application_name = 'session-value';
SET LOCAL application_name = 'local-value';
SHOW application_name;
COMMIT;
SHOW application_name;

-- Local scope outside an explicit transaction ends with the statement.
SET LOCAL application_name = 'one-statement';
SHOW application_name;
SELECT set_config('application_name', 'one-function', true);
SHOW application_name;

-- Rolling back to a savepoint restores both tracks.
BEGIN;
SAVEPOINT s;
SET application_name = 'after-savepoint';
SET LOCAL search_path = private;
ROLLBACK TO s;
SELECT current_setting('application_name'), current_setting('search_path');
COMMIT;
SHOW application_name;

-- Releasing instead keeps both changes.
BEGIN;
SAVEPOINT s;
SET application_name = 'released-session';
SET LOCAL search_path = released_local;
RELEASE s;
SELECT current_setting('application_name'), current_setting('search_path');
COMMIT;
SELECT current_setting('application_name'), current_setting('search_path');

-- RESET one and RESET ALL use the connection's startup defaults.
SET application_name = 'reset-me';
RESET application_name;
SHOW application_name;
SET application_name = 'reset-all-me';
SET search_path = private;
RESET ALL;
SELECT current_setting('application_name'), current_setting('search_path');

-- Error surfaces: NULL name (22004), unknown setting (42704), read-only
-- setting (55P02), and invalid value (22023).
SELECT set_config(NULL, 'x', false);
SELECT set_config('no_such_setting_xyz', 'x', false);
SELECT set_config('server_version', 'x', false);
RESET no_such_setting_xyz;
SET extra_float_digits = 99;
