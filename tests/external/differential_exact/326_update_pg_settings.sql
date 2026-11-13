-- UPDATE pg_settings runs PostgreSQL's pg_settings_u rule: set_config for
-- each selected row whose name the statement keeps, then UPDATE 0.
UPDATE pg_settings SET setting = '2' WHERE name = 'extra_float_digits';
SHOW extra_float_digits;
UPDATE pg_settings s SET setting = 'rule_app' WHERE s.name = 'application_name';
SHOW application_name;
UPDATE pg_settings SET setting = 'x' WHERE name = 'no_such_setting';
UPDATE pg_settings SET name = 'other', setting = '0' WHERE name = 'extra_float_digits';
SHOW extra_float_digits;
BEGIN;
UPDATE pg_settings SET setting = '3' WHERE name = 'extra_float_digits';
ROLLBACK;
SHOW extra_float_digits;
UPDATE pg_settings SET setting = '9' WHERE name = 'extra_float_digits';
UPDATE pg_settings SET setting = 'zz' WHERE name = 'extra_float_digits';
UPDATE pg_settings SET nonexistent = 'zz' WHERE name = 'extra_float_digits';
UPDATE pg_settings SET setting = '1' WHERE name = 'extra_float_digits' RETURNING name;
DELETE FROM pg_settings;
INSERT INTO pg_settings (name) VALUES ('x');
RESET extra_float_digits;
RESET application_name;
