SET client_min_messages = notice;
CREATE FUNCTION completion_hook() RETURNS event_trigger LANGUAGE plpgsql AS
  'BEGIN RAISE NOTICE ''hook notice''; RAISE EXCEPTION USING ERRCODE=''ZX123'', MESSAGE=''hook rejected''; RETURN; END';
CREATE EVENT TRIGGER completion_end ON ddl_command_end
  WHEN TAG IN ('CREATE TABLE') EXECUTE FUNCTION completion_hook();
CREATE TABLE completion_direct(value integer);
DO 'BEGIN CREATE TABLE completion_nested(value integer); END';
DO 'BEGIN EXECUTE ''CREATE TABLE completion_dynamic(value integer)''; END';
SELECT to_regclass('completion_direct') IS NULL
  AND to_regclass('completion_nested') IS NULL
  AND to_regclass('completion_dynamic') IS NULL;
CREATE OR REPLACE FUNCTION completion_hook() RETURNS event_trigger LANGUAGE plpgsql AS
  'BEGIN RAISE NOTICE ''hook success''; RETURN; END';
CREATE TABLE completion_success(value integer);
DROP EVENT TRIGGER completion_end;
DROP FUNCTION completion_hook();
DROP TABLE completion_success;
RESET client_min_messages;
