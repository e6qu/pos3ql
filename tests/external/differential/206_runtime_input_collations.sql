-- Runtime SQL-body checks use declared argument types and input collation.
-- PostgreSQL REL_18_6 src/backend/utils/cache/funccache.c:268-300.
CREATE SEQUENCE body_collation_checks;
CREATE TYPE body_collation_choice AS ENUM('a');
CREATE DOMAIN body_collation_checked AS body_collation_choice
  CHECK(nextval('body_collation_checks')>0);
CREATE FUNCTION body_collation(input text) RETURNS body_collation_checked[]
  LANGUAGE sql SET search_path=public AS 'SELECT ''{a}''::body_collation_checked[]';
CREATE FUNCTION body_collation_cast(input text) RETURNS integer
  LANGUAGE sql SET search_path=public AS 'SELECT cardinality(''{a}''::body_collation_checked[])';
CREATE CAST(text AS integer) WITH FUNCTION body_collation_cast(text) AS ASSIGNMENT;
CREATE TABLE body_collation_numbers(value integer);
CREATE TABLE body_collation_texts(value text COLLATE "C");
INSERT INTO body_collation_texts VALUES('a');
CREATE FUNCTION body_collation_default(input text DEFAULT ('a' COLLATE "C"))
  RETURNS body_collation_checked[] LANGUAGE sql SET search_path=public
  AS 'SELECT ''{a}''::body_collation_checked[]';
CREATE FUNCTION body_collation_pair(first_input text, second_input text DEFAULT ('a' COLLATE "POSIX"))
  RETURNS body_collation_checked[] LANGUAGE sql SET search_path=public
  AS 'SELECT ''{a}''::body_collation_checked[]';
CREATE FUNCTION body_collation_srf(input text DEFAULT ('a' COLLATE "C"))
  RETURNS SETOF body_collation_checked[] LANGUAGE sql SET search_path=public
  AS 'SELECT ''{a}''::body_collation_checked[]';
CREATE PROCEDURE body_collation_procedure(input text DEFAULT ('a' COLLATE "C"))
  LANGUAGE sql SET search_path=public AS 'SELECT ''{a}''::body_collation_checked[]';
CREATE FUNCTION body_collation_identity(input text) RETURNS text LANGUAGE sql AS 'SELECT input';
CREATE FUNCTION body_collation_nested(input text DEFAULT body_collation_identity('a' COLLATE "C"))
  RETURNS body_collation_checked[] LANGUAGE sql SET search_path=public
  AS 'SELECT ''{a}''::body_collation_checked[]';
ALTER SEQUENCE body_collation_checks RESTART WITH 1;
SELECT body_collation('first' COLLATE "C");
SELECT 'first_c',last_value FROM body_collation_checks;
SELECT body_collation('second'::varchar COLLATE "C");
SELECT 'same_declared_type',last_value FROM body_collation_checks;
SELECT body_collation('third' COLLATE "POSIX");
SELECT 'posix',last_value FROM body_collation_checks;
SELECT body_collation('fourth'::text);
SELECT body_collation('fifth'::varchar);
SELECT 'default_collation',last_value FROM body_collation_checks;
SELECT body_collation(NULL::text COLLATE "C");
SELECT 'null_reuses_c',last_value FROM body_collation_checks;
SELECT ('42'::text COLLATE "C")::integer;
SELECT ('43'::text COLLATE "C")::integer;
SELECT 'cast_c',last_value FROM body_collation_checks;
SELECT ('44'::text COLLATE "POSIX")::integer;
SELECT 'cast_posix',last_value FROM body_collation_checks;
SELECT ARRAY['45'::text COLLATE "C", '46'::text COLLATE "C"]::integer[];
SELECT 'array_cast_reuses_c',last_value FROM body_collation_checks;
INSERT INTO body_collation_numbers
  SELECT '47'::text COLLATE "C" FROM generate_series(1,2);
INSERT INTO body_collation_numbers VALUES('48'::text COLLATE "POSIX");
SELECT 'assignment_reuses',last_value FROM body_collation_checks;
SELECT value FROM body_collation_numbers ORDER BY value;
SELECT body_collation_default();
SELECT body_collation_default('a' COLLATE "C");
SELECT 'default_reuses_explicit_c',last_value FROM body_collation_checks;
SELECT body_collation_default('a'::text);
SELECT 'default_is_separate',last_value FROM body_collation_checks;
SELECT body_collation_pair(value) FROM body_collation_texts;
SELECT body_collation_pair('a' COLLATE "POSIX");
SELECT 'default_explicit_beats_argument_implicit',last_value FROM body_collation_checks;
SELECT * FROM body_collation_srf();
SELECT * FROM body_collation_srf('a' COLLATE "C");
SELECT 'srf_default_reuses_c',last_value FROM body_collation_checks;
SELECT * FROM body_collation_srf('a'::text);
SELECT 'srf_default_is_separate',last_value FROM body_collation_checks;
CALL body_collation_procedure();
CALL body_collation_procedure('a' COLLATE "C");
SELECT 'procedure_default_reuses_c',last_value FROM body_collation_checks;
CALL body_collation_procedure('a'::text);
SELECT 'procedure_default_is_separate',last_value FROM body_collation_checks;
SELECT body_collation_pair('a' COLLATE "C");
SELECT 'default_explicit_conflict_before_checks',last_value FROM body_collation_checks;
SELECT body_collation_nested();
SELECT body_collation_nested('a' COLLATE "C");
SELECT 'nested_default_reuses_c',last_value FROM body_collation_checks;
SELECT body_collation_nested('a'::text);
SELECT 'nested_default_is_separate',last_value FROM body_collation_checks;
DROP FUNCTION body_collation_nested(text);
DROP FUNCTION body_collation_identity(text);
DROP PROCEDURE body_collation_procedure(text);
DROP FUNCTION body_collation_default(text),body_collation_pair(text,text),body_collation_srf(text);
DROP TABLE body_collation_texts;
DROP TABLE body_collation_numbers;
DROP CAST(text AS integer);
DROP FUNCTION body_collation(text),body_collation_cast(text);
DROP DOMAIN body_collation_checked;
DROP TYPE body_collation_choice;
DROP SEQUENCE body_collation_checks;
