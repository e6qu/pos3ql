-- current_setting(name [, missing_ok]) — the function form of SHOW, returning
-- a setting's value as text. Matches PostgreSQL 18. (server_version / _num are
-- intentionally omitted: their value is release-specific.)
--
-- Values that are stable across releases:
SELECT current_setting('client_encoding');
SELECT current_setting('server_encoding');
SELECT current_setting('standard_conforming_strings');
SELECT current_setting('integer_datetimes');
SELECT current_setting('TimeZone');
SELECT current_setting('DateStyle');
SELECT current_setting('search_path');

-- Case-insensitive setting name, like SHOW.
SELECT current_setting('timezone');
SELECT current_setting('datestyle');

-- Composes in an expression and under other functions.
SELECT upper(current_setting('client_encoding'));
SELECT current_setting('client_encoding') || '/' || current_setting('server_encoding');

-- Reflects a SET earlier in the same session.
SET search_path = myschema, public;
SELECT current_setting('search_path');
SET TimeZone = 'UTC';
SELECT current_setting('TimeZone');

-- An unknown setting errors 42704, unless missing_ok is true (then NULL).
SELECT current_setting('no_such_setting_xyz');
SELECT current_setting('no_such_setting_xyz', true) IS NULL;
SELECT current_setting('no_such_setting_xyz', false);
