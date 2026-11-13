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
-- Inside routine bodies the rule's set_config rows go nowhere and the
-- statement reports no rows.
DO $$ DECLARE n int; BEGIN
  UPDATE pg_settings SET setting = '2' WHERE name = 'extra_float_digits';
  GET DIAGNOSTICS n = ROW_COUNT;
  RAISE NOTICE '% %', current_setting('extra_float_digits'), n;
END $$;
CREATE FUNCTION settings_in_sql() RETURNS text LANGUAGE sql AS $$
  UPDATE pg_settings SET setting = '3' WHERE name = 'extra_float_digits';
  SELECT current_setting('extra_float_digits') $$;
SELECT settings_in_sql();
DO $$ BEGIN
  EXECUTE 'UPDATE pg_settings SET setting = ''0'' WHERE name = ''extra_float_digits''';
  RAISE NOTICE '%', current_setting('extra_float_digits');
END $$;
DO $$ BEGIN DELETE FROM pg_settings; END $$;
DO $$ BEGIN INSERT INTO pg_settings (name) VALUES ('x'); END $$;
DROP FUNCTION settings_in_sql();
RESET extra_float_digits;
