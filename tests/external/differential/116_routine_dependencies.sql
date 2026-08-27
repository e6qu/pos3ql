DROP TABLE IF EXISTS routine_dependency_target, routine_dependency_source CASCADE;
DROP FUNCTION IF EXISTS routine_dependency_pick(integer) CASCADE;
DROP FUNCTION IF EXISTS routine_dependency_pick(text) CASCADE;

CREATE TABLE routine_dependency_target(id integer PRIMARY KEY, value integer);
CREATE TABLE routine_dependency_source(id integer, value integer);
CREATE FUNCTION routine_dependency_pick(value integer) RETURNS integer
  LANGUAGE SQL RETURN value + 1;
CREATE FUNCTION routine_dependency_pick(value text) RETURNS integer
  LANGUAGE SQL RETURN 900;
CREATE PROCEDURE routine_dependency_upsert(input_id integer, input_value integer)
  LANGUAGE SQL
  BEGIN ATOMIC
    INSERT INTO routine_dependency_target VALUES (input_id, input_value)
      ON CONFLICT (id) DO UPDATE
      SET value = routine_dependency_pick(excluded.value);
    UPDATE routine_dependency_target
      SET value = routine_dependency_pick(value)
      WHERE id = input_id;
  END;
CREATE PROCEDURE routine_dependency_merge() LANGUAGE SQL
  BEGIN ATOMIC
    MERGE INTO routine_dependency_target AS target
    USING routine_dependency_source AS source
    ON target.id = source.id
    WHEN MATCHED THEN
      UPDATE SET value = routine_dependency_pick(source.value)
    WHEN NOT MATCHED THEN
      INSERT (id, value)
      VALUES (source.id, routine_dependency_pick(source.value));
  END;

ALTER FUNCTION routine_dependency_pick(integer)
  RENAME TO routine_dependency_pick_integer;
CALL routine_dependency_upsert(1, 4);
CALL routine_dependency_upsert(1, 8);
INSERT INTO routine_dependency_source VALUES (1, 20), (2, 30);
CALL routine_dependency_merge();
SELECT id, value FROM routine_dependency_target ORDER BY id;

ALTER TABLE routine_dependency_source DROP COLUMN value RESTRICT;
ALTER TABLE routine_dependency_source DROP COLUMN value CASCADE;
SELECT count(*) FROM pg_proc WHERE proname = 'routine_dependency_merge';
DROP FUNCTION routine_dependency_pick_integer(integer) RESTRICT;
DROP FUNCTION routine_dependency_pick_integer(integer) CASCADE;
SELECT count(*) FROM pg_proc
 WHERE proname IN ('routine_dependency_upsert', 'routine_dependency_pick_integer');

DROP FUNCTION routine_dependency_pick(text);
DROP TABLE routine_dependency_target, routine_dependency_source;
