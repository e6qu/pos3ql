-- PostgreSQL knows the plpgsql.* parameters only once PL/pgSQL loads in the
-- session: before that they are ordinary placeholders that take any value,
-- and loading turns each into the parameter, warning about a value it
-- rejects or that the role setting it could not set.
SHOW plpgsql.variable_conflict;
SELECT current_setting('plpgsql.extra_warnings', true) IS NULL AS unknown;
SELECT count(*) FROM pg_settings WHERE name LIKE 'plpgsql%';
SET plpgsql.print_strict_params = 'maybe';
SET plpgsql.extra_warnings = 'all';
SET plpgsql.extra_errors = too_many_rows, strict_multi_assignment;
SELECT set_config('plpgsql.extra_errors', 'too_many_rows', false);
BEGIN;
SET LOCAL plpgsql.check_asserts = off;
SHOW plpgsql.check_asserts;
COMMIT;
SHOW plpgsql.extra_warnings;
SELECT name, setting FROM pg_settings WHERE name LIKE 'plpgsql%';
CREATE ROLE pl_load_plain;
GRANT CREATE ON SCHEMA public TO pl_load_plain;
SET ROLE pl_load_plain;
SET plpgsql.variable_conflict = 'use_column';
SHOW plpgsql.variable_conflict;
DO $$ BEGIN RAISE NOTICE 'loaded'; END $$;
RESET ROLE;
SHOW plpgsql.variable_conflict;
SHOW plpgsql.print_strict_params;
SHOW plpgsql.extra_warnings;
SHOW plpgsql.extra_errors;
SELECT name, setting, source FROM pg_settings WHERE name LIKE 'plpgsql%' ORDER BY 1;
SET plpgsql.extra_warnings = too_many_rows, "strict_multi_assignment";
SHOW plpgsql.extra_warnings;
SET plpgsql.extra_warnings = '"Too_Many_Rows" ,  shadowed_variables';
SHOW plpgsql.extra_warnings;
SET plpgsql.extra_warnings = '';
SHOW plpgsql.extra_warnings;
SET plpgsql.extra_warnings = 'too_many_rows,,x';
SET plpgsql.extra_warnings = 'too_many_rows x';
SET plpgsql.extra_warnings = '"unterminated';
SET plpgsql.extra_warnings = '"all"';
SET plpgsql.extra_warnings = 'too_many_rows,"none"';
RESET plpgsql.extra_warnings;
SHOW plpgsql.extra_warnings;
SET plpgsql.variable_conflict = 'bogus';
-- A placeholder that is not one of PL/pgSQL's parameters is gone.
SET plpgsql.other = 'x';
