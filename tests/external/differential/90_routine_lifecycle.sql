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

CREATE TABLE routine_replace_target (id integer PRIMARY KEY);
CREATE FUNCTION routine_replace_gate() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
CREATE TRIGGER routine_replace_gate BEFORE INSERT ON routine_replace_target
  FOR EACH ROW EXECUTE FUNCTION routine_replace_gate();
CREATE TABLE routine_replace_reference (fn regprocedure);
INSERT INTO routine_replace_reference VALUES ('routine_replace_gate()'::regprocedure);
BEGIN;
SAVEPOINT before_replacement;
CREATE OR REPLACE FUNCTION routine_replace_gate() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NULL; END';
INSERT INTO routine_replace_target VALUES (1);
ROLLBACK TO SAVEPOINT before_replacement;
INSERT INTO routine_replace_target VALUES (1);
COMMIT;
CREATE OR REPLACE FUNCTION routine_replace_gate() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NULL; END';
INSERT INTO routine_replace_target VALUES (2);
SELECT p.oid = (SELECT fn::oid FROM routine_replace_reference),
       (SELECT count(*) FROM routine_replace_target)
  FROM pg_proc p
 WHERE p.proname = 'routine_replace_gate';
DROP TABLE routine_replace_target, routine_replace_reference;
DROP FUNCTION routine_replace_gate();
