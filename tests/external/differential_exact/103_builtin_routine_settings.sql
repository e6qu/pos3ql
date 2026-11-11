-- SET and RESET change a built-in function's proconfig in the current
-- database; as in PostgreSQL, a built-in call does not apply them.
ALTER FUNCTION current_setting(text) SET statement_timeout = '5s';
ALTER FUNCTION current_setting(text) SET search_path = pg_catalog, public;
SELECT proconfig FROM pg_proc WHERE oid = 'current_setting(text)'::regprocedure;
SELECT current_setting('statement_timeout'), current_setting('search_path');
ALTER FUNCTION current_setting(text) SET statement_timeout = '7s';
SELECT proconfig FROM pg_proc WHERE oid = 'current_setting(text)'::regprocedure;
ALTER FUNCTION current_setting(text) RESET statement_timeout;
SELECT proconfig FROM pg_proc WHERE oid = 'current_setting(text)'::regprocedure;
ALTER FUNCTION current_setting(text) SET datestyle FROM CURRENT;
SELECT proconfig FROM pg_proc WHERE oid = 'current_setting(text)'::regprocedure;
ALTER FUNCTION current_setting(text) RESET ALL;
SELECT proconfig FROM pg_proc WHERE oid = 'current_setting(text)'::regprocedure;
ALTER FUNCTION upper(text) SET no_such_setting = 1;
ALTER FUNCTION upper(text) SET statement_timeout = 'abc';
BEGIN;
ALTER FUNCTION upper(text) SET statement_timeout = '1s';
ROLLBACK;
SELECT proconfig FROM pg_proc WHERE oid = 'upper(text)'::regprocedure;
