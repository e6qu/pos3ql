-- A trigger function called directly resolves as a function returning
-- trigger and fails only when the call runs, as PostgreSQL's PL/pgSQL
-- compiler and built-in trigger functions reject it.
CREATE SCHEMA trigger_function_calls;
SET search_path = trigger_function_calls, public;
CREATE TABLE t (a int);
CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
CREATE FUNCTION etf() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN END $$;
SELECT tf();
SELECT etf();
SELECT tf() FROM t;
SELECT 1 WHERE false AND tf() IS NULL;
SELECT * FROM tf();
SELECT * FROM etf();
CREATE FUNCTION calls_tf() RETURNS int LANGUAGE plpgsql AS $$
BEGIN PERFORM tf(); RETURN 1; END $$;
SELECT calls_tf();
SELECT suppress_redundant_updates_trigger();
SELECT unique_key_recheck();
SELECT "RI_FKey_check_ins"();
SELECT "RI_FKey_cascade_del"();
SELECT tsvector_update_trigger();
RESET search_path;
DROP SCHEMA trigger_function_calls CASCADE;
