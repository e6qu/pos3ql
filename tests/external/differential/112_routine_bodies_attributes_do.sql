DROP TABLE IF EXISTS routine_attribute_log CASCADE;
DROP ROLE IF EXISTS routine_body_caller;

CREATE TABLE routine_attribute_log(value text);
CREATE FUNCTION standard_return(a integer) RETURNS integer
  LANGUAGE SQL IMMUTABLE PARALLEL SAFE COST 2
  SET application_name TO 'inside' RETURN a + 1;
CREATE FUNCTION standard_atomic(a integer) RETURNS integer
  LANGUAGE SQL BEGIN ATOMIC SELECT a + 2; END;
CREATE FUNCTION standard_mutable() RETURNS text
  LANGUAGE SQL SECURITY DEFINER SET application_name TO 'mutable scope'
  BEGIN ATOMIC
    INSERT INTO routine_attribute_log
      VALUES (current_user || ':' || current_setting('application_name'))
      RETURNING current_user || ':' || current_setting('application_name');
  END;
CREATE FUNCTION inner_config() RETURNS text
  LANGUAGE SQL SET application_name TO 'inner'
  RETURN current_setting('application_name');
CREATE FUNCTION nested_config() RETURNS text
  LANGUAGE SQL SET application_name TO 'outer'
  RETURN current_setting('application_name') || ':' || inner_config()
         || ':' || current_setting('application_name');
CREATE FUNCTION persistent_config() RETURNS text
  LANGUAGE SQL SET application_name TO 'temporary'
  AS 'SET application_name TO ''persisted'';
      SELECT current_setting(''application_name'')';

CREATE ROLE routine_body_caller;
SET application_name TO 'outside';
SET ROLE routine_body_caller;
SELECT standard_return(4), standard_atomic(4), standard_mutable(),
       current_setting('application_name');
RESET ROLE;
SELECT nested_config(), current_setting('application_name');
SELECT persistent_config(), current_setting('application_name');
SET application_name TO 'outside';
SET application_name TO 'captured';
ALTER FUNCTION standard_atomic(integer) SET application_name FROM CURRENT;
SET application_name TO 'outside';
SELECT proconfig::text FROM pg_proc WHERE proname = 'standard_atomic';
ALTER FUNCTION standard_atomic(integer) RESET application_name;
SELECT proconfig IS NULL FROM pg_proc WHERE proname = 'standard_atomic';

ALTER FUNCTION standard_return(integer)
  VOLATILE PARALLEL RESTRICTED COST 7 SET application_name TO 'altered';
SELECT standard_return(9), current_setting('application_name');
SELECT provolatile, proparallel, prosecdef, proleakproof, procost, prorows,
       proconfig::text, prosqlbody IS NULL
  FROM pg_proc WHERE proname = 'standard_return';
SELECT pg_typeof(proargtypes)::text, pg_typeof(protrftypes)::text,
       pg_typeof(prosupport)::text, probin IS NULL, prosupport::oid
  FROM pg_proc WHERE proname = 'standard_return';
SELECT pg_get_functiondef('standard_return(integer)'::regprocedure)
         LIKE '%SET application_name TO ''altered''%';

DO 'BEGIN INSERT INTO routine_attribute_log VALUES (''anonymous''); END';
SELECT value FROM routine_attribute_log ORDER BY value;

DROP TABLE routine_attribute_log CASCADE;
DROP ROLE routine_body_caller;
