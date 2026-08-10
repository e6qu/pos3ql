DROP SCHEMA IF EXISTS routine_lifecycle_target CASCADE;
DROP ROUTINE IF EXISTS routine_lifecycle(integer);

CREATE SCHEMA routine_lifecycle_target;
CREATE FUNCTION routine_lifecycle(integer) RETURNS integer LANGUAGE SQL AS 'SELECT 1';
CREATE PROCEDURE routine_lifecycle(text) LANGUAGE SQL AS 'SELECT 1';

BEGIN;
ALTER ROUTINE routine_lifecycle(integer) RENAME TO routine_renamed;
ALTER PROCEDURE routine_lifecycle(text) SET SCHEMA routine_lifecycle_target;
SELECT proname, nspname, prokind
  FROM pg_proc JOIN pg_namespace ON pronamespace = pg_namespace.oid
 WHERE proname IN ('routine_lifecycle', 'routine_renamed')
 ORDER BY proname, nspname, prokind;
ROLLBACK;

ALTER FUNCTION routine_lifecycle(integer) RENAME TO routine_renamed;
ALTER PROCEDURE routine_lifecycle(text) SET SCHEMA routine_lifecycle_target;
SELECT routine_renamed(1);
SELECT proname, nspname, prokind
  FROM pg_proc JOIN pg_namespace ON pronamespace = pg_namespace.oid
 WHERE proname IN ('routine_lifecycle', 'routine_renamed')
 ORDER BY proname, nspname, prokind;

DROP ROUTINE routine_lifecycle_target.routine_lifecycle(text), routine_renamed(integer);
SELECT count(*) FROM pg_proc WHERE proname IN ('routine_lifecycle', 'routine_renamed');
DROP SCHEMA routine_lifecycle_target;
