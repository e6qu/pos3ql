-- PL/pgSQL reserves the "plpgsql" parameter prefix when it first runs in a
-- session: its placeholders are removed with a warning and new ones fail.
-- Its own parameters then show in pg_settings, and an extended-protocol
-- SHOW keeps a custom parameter's defining spelling as the column title.
SET plpgsql.foo = 'x';
SHOW plpgsql.foo;
SET plpgsql.print_strict_params = on;
DO $$ BEGIN RAISE NOTICE 'hi'; END $$;
SHOW plpgsql.foo;
SET plpgsql.bar = 'y';
SELECT current_setting('plpgsql.bar', true);
SHOW plpgsql.print_strict_params;
SET plpgsql.variable_conflict = 'use_column';
SHOW plpgsql.variable_conflict;
RESET plpgsql.variable_conflict;
RESET plpgsql.print_strict_params;
SELECT name, setting, category, short_desc, context, vartype, boot_val FROM pg_settings WHERE name LIKE 'plpgsql%' ORDER BY 1;
DO $$ BEGIN ASSERT 1 = 2, 'boom'; END $$;
SET plpgsql.check_asserts = off;
DO $$ BEGIN ASSERT 1 = 2, 'boom'; END $$;
SHOW plpgsql.check_asserts;
RESET plpgsql.check_asserts;
SET plpgsql.extra_warnings = 'foo';
SET plpgsql.extra_warnings = 'all, none';
SET plpgsql.extra_errors = 'too_many_rows,';
SET plpgsql.extra_warnings = 'NONE';
SHOW plpgsql.extra_warnings;
RESET plpgsql.extra_warnings;
SET "MyApp.Quoted" = 'q';
SHOW "MyApp.Quoted" \bind \g
SHOW "myapp.quoted" \bind \g
SET myApp.Mixed = 'v';
SHOW "myApp.Mixed" \bind \g
