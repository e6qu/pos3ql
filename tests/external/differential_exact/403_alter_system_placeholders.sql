-- ALTER SYSTEM changes no session: an unknown custom parameter's value is
-- not checked and makes no placeholder, and resetting one with a valid
-- name is allowed even under PL/pgSQL's reserved prefix. Once PL/pgSQL has
-- loaded, its parameters' values are checked.
\c
ALTER SYSTEM SET plpgsql.extra_warnings = 'bogus';
ALTER SYSTEM SET plpgsql.variable_conflict = 'bogus';
ALTER SYSTEM SET plpgsql.other = 'x';
ALTER SYSTEM SET plpgsql.print_strict_params = 'maybe';
ALTER SYSTEM SET foo.baz = 'y';
SHOW foo.baz;
ALTER SYSTEM RESET plpgsql.extra_warnings;
ALTER SYSTEM RESET plpgsql.other;
ALTER SYSTEM RESET plpgsql.variable_conflict;
ALTER SYSTEM RESET plpgsql.print_strict_params;
ALTER SYSTEM RESET foo.baz;
DO $$ BEGIN NULL; END $$;
ALTER SYSTEM SET plpgsql.extra_warnings = 'bogus';
ALTER SYSTEM SET plpgsql.extra_warnings = 'all';
ALTER SYSTEM SET plpgsql.variable_conflict = 'bogus';
ALTER SYSTEM SET plpgsql.other = 'x';
ALTER SYSTEM SET plpgsql.print_strict_params = 'maybe';
ALTER SYSTEM SET plpgsql.check_asserts = off;
ALTER SYSTEM RESET plpgsql.other;
ALTER SYSTEM SET "foo.1bad" = 'x';
ALTER SYSTEM RESET "foo.1bad";
ALTER SYSTEM SET nodot = 1;
ALTER SYSTEM RESET nodot;
CREATE ROLE asr;
ALTER ROLE asr SET plpgsql.other2 = 'x';
ALTER ROLE asr RESET plpgsql.other2;
DROP ROLE asr;
ALTER SYSTEM RESET ALL;
\c
-- Loading PL/pgSQL reapplies each transaction level's own value.
SET plpgsql.extra_warnings = 'bad';
BEGIN;
SET plpgsql.extra_warnings = 'bad2';
SAVEPOINT s;
SET LOCAL plpgsql.extra_warnings = 'bad2';
DO $$ BEGIN NULL; END $$;
COMMIT;
SHOW plpgsql.extra_warnings;
