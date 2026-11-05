-- Caught errors keep procedural variables and sequence effects, while undoing
-- data, schema and settings changed inside the failed block.
CREATE TABLE subtx_values(id integer PRIMARY KEY, value integer);
INSERT INTO subtx_values VALUES (1,10),(2,20),(3,30);
CREATE TABLE subtx_notes(label text, value text);
CREATE SEQUENCE subtx_attempts;
SET application_name = 'kept';
SET DateStyle = 'ISO, MDY';
CREATE FUNCTION subtx_restore() RETURNS integer LANGUAGE plpgsql AS $$
DECLARE kept integer := 1;
BEGIN
  BEGIN
    kept := 9;
    PERFORM nextval('subtx_attempts');
    PERFORM set_config('application_name','discarded',false);
    PERFORM set_config('DateStyle','SQL, DMY',true);
    CREATE TABLE subtx_discarded(id integer PRIMARY KEY);
    ALTER TABLE subtx_values ADD COLUMN discarded integer;
    CREATE INDEX subtx_discarded_index ON subtx_values(value);
    INSERT INTO subtx_values(id,value) VALUES (4,40);
    RAISE EXCEPTION 'undo this block';
  EXCEPTION WHEN raise_exception THEN
    INSERT INTO subtx_notes VALUES
      ('handler',kept::text || ':' || current_setting('application_name') || ':' || current_setting('DateStyle'));
  END;
  RETURN kept;
END$$;
SELECT subtx_restore();
SELECT label,value FROM subtx_notes ORDER BY label;
SELECT count(*),sum(value) FROM subtx_values;
SELECT to_regclass('subtx_discarded') IS NULL,
       to_regclass('subtx_discarded_index') IS NULL,
       NOT EXISTS (SELECT 1 FROM pg_attribute WHERE attrelid='subtx_values'::regclass AND attname='discarded' AND NOT attisdropped);
SELECT last_value,is_called FROM subtx_attempts;

-- A caught DROP must restore the existing relation and its stored data.
DO $$BEGIN
  BEGIN
    DROP TABLE subtx_values;
    RAISE EXCEPTION 'restore the table';
  EXCEPTION WHEN raise_exception THEN
    INSERT INTO subtx_notes VALUES ('drop', (SELECT sum(value)::text FROM subtx_values));
  END;
END$$;
SELECT label,value FROM subtx_notes WHERE label='drop';

-- Nested exception blocks must not consume or replace the client's savepoint.
BEGIN;
SAVEPOINT client_boundary;
DO $$BEGIN
  PERFORM set_config('application_name','outer',false);
  BEGIN
    PERFORM set_config('application_name','inner',false);
    BEGIN
      PERFORM set_config('application_name','deep',false);
      RAISE EXCEPTION 'deep failure';
    EXCEPTION WHEN raise_exception THEN
      INSERT INTO subtx_notes VALUES ('deep',current_setting('application_name'));
    END;
    RAISE EXCEPTION 'inner failure';
  EXCEPTION WHEN raise_exception THEN
    INSERT INTO subtx_notes VALUES ('inner',current_setting('application_name'));
  END;
END$$;
SELECT label,value FROM subtx_notes WHERE label IN ('deep','inner') ORDER BY label;
ROLLBACK TO client_boundary;
SELECT current_setting('application_name'),count(*) FROM subtx_notes WHERE label IN ('deep','inner');
RELEASE client_boundary;
COMMIT;

-- Aborted rows must leave no queued deferred trigger, even after many catches.
CREATE TABLE subtx_deferred(id integer);
CREATE TABLE subtx_delivered(id integer);
CREATE FUNCTION subtx_deliver() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN INSERT INTO subtx_delivered VALUES (NEW.id); RETURN NEW; END$$;
CREATE CONSTRAINT TRIGGER subtx_deliver AFTER INSERT ON subtx_deferred
DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION subtx_deliver();
BEGIN;
DO $$BEGIN
  FOR attempt IN 1..64 LOOP
    BEGIN
      INSERT INTO subtx_deferred VALUES (-attempt);
      RAISE EXCEPTION 'discard the queued event';
    EXCEPTION WHEN raise_exception THEN NULL;
    END;
  END LOOP;
END$$;
INSERT INTO subtx_deferred VALUES (7);
COMMIT;
SELECT id FROM subtx_deferred ORDER BY id;
SELECT id FROM subtx_delivered ORDER BY id;

-- Cursor creation rolls back, but motion of an older cursor does not.
BEGIN;
DECLARE subtx_old NO SCROLL CURSOR FOR SELECT id FROM subtx_values ORDER BY id;
SAVEPOINT cursor_boundary;
DECLARE subtx_new CURSOR FOR SELECT 41;
FETCH NEXT FROM subtx_old;
CLOSE subtx_new;
DECLARE subtx_new CURSOR FOR SELECT 42;
ROLLBACK TO cursor_boundary;
SELECT name FROM pg_cursors WHERE name IN ('subtx_old','subtx_new') ORDER BY name;
FETCH NEXT FROM subtx_old;
RELEASE cursor_boundary;
COMMIT;

-- A procedural cursor name remains in its variable after its portal is undone.
CREATE FUNCTION subtx_cursor() RETURNS integer LANGUAGE plpgsql AS $$
DECLARE selected refcursor := 'subtx_procedural'; answer integer;
BEGIN
  BEGIN
    OPEN selected FOR SELECT 11;
    RAISE EXCEPTION 'close the new cursor';
  EXCEPTION WHEN raise_exception THEN
    OPEN selected FOR SELECT 12;
  END;
  FETCH selected INTO answer;
  CLOSE selected;
  RETURN answer;
END$$;
SELECT subtx_cursor();
SELECT current_setting('application_name'),current_setting('DateStyle');
