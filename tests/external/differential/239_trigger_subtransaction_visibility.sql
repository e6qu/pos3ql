CREATE TABLE trigger_subtx_source(id integer PRIMARY KEY);
CREATE TABLE trigger_subtx_notes(kind text,id integer,seen text);
CREATE TABLE trigger_subtx_pending(id integer);
CREATE TABLE trigger_subtx_delivered(id integer);
SET application_name = 'kept';
CREATE FUNCTION trigger_subtx_deliver() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN INSERT INTO trigger_subtx_delivered VALUES(NEW.id); RETURN NEW; END$$;
CREATE CONSTRAINT TRIGGER trigger_subtx_deliver AFTER INSERT ON trigger_subtx_pending
DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION trigger_subtx_deliver();
CREATE FUNCTION trigger_subtx_before() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  BEGIN
    INSERT INTO trigger_subtx_notes VALUES('discarded',NEW.id,'before');
    PERFORM set_config('application_name','discarded-before',false);
    NEW.id := NEW.id + 10;
    RAISE EXCEPTION 'keep NEW but undo the writes';
  EXCEPTION WHEN raise_exception THEN
    INSERT INTO trigger_subtx_notes VALUES('before',NEW.id,current_setting('application_name'));
  END;
  RETURN NEW;
END$$;
CREATE FUNCTION trigger_subtx_after() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  BEGIN
    INSERT INTO trigger_subtx_pending VALUES(NEW.id);
    PERFORM set_config('application_name','discarded-after',false);
    RAISE EXCEPTION 'discard the deferred event';
  EXCEPTION WHEN raise_exception THEN
    INSERT INTO trigger_subtx_notes VALUES('after',NEW.id,current_setting('application_name'));
  END;
  RETURN NEW;
END$$;
CREATE FUNCTION trigger_subtx_statement() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  BEGIN
    INSERT INTO trigger_subtx_pending SELECT id FROM inserted;
    PERFORM set_config('application_name','discarded-statement',false);
    RAISE EXCEPTION 'keep transition rows but undo their writes';
  EXCEPTION WHEN raise_exception THEN
    INSERT INTO trigger_subtx_notes SELECT 'statement',count(*)::integer,current_setting('application_name') FROM inserted;
  END;
  RETURN NULL;
END$$;
CREATE TRIGGER trigger_subtx_before BEFORE INSERT ON trigger_subtx_source
FOR EACH ROW EXECUTE FUNCTION trigger_subtx_before();
CREATE TRIGGER trigger_subtx_after AFTER INSERT ON trigger_subtx_source
FOR EACH ROW EXECUTE FUNCTION trigger_subtx_after();
CREATE TRIGGER trigger_subtx_statement AFTER INSERT ON trigger_subtx_source
REFERENCING NEW TABLE AS inserted FOR EACH STATEMENT EXECUTE FUNCTION trigger_subtx_statement();
BEGIN;
INSERT INTO trigger_subtx_source VALUES(1),(2);
INSERT INTO trigger_subtx_pending VALUES(99);
COMMIT;
SELECT id FROM trigger_subtx_source ORDER BY id;
SELECT kind,id,seen FROM trigger_subtx_notes ORDER BY kind,id;
SELECT id FROM trigger_subtx_pending ORDER BY id;
SELECT id FROM trigger_subtx_delivered ORDER BY id;
SELECT current_setting('application_name');
