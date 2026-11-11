-- Row triggers fired inside a write whose bodies need the statement executor:
-- DDL, writes to views and ruled tables, procedures that run DDL, and
-- routine calls that read the trigger's own earlier writes. Each body runs
-- once per firing, and NEW and OLD are NULL records where a firing has no row.
CREATE SCHEMA executor_triggers;
SET search_path = executor_triggers, public;
CREATE SEQUENCE s;
CREATE TABLE t(id int PRIMARY KEY, v text, n bigint);
CREATE TABLE log(msg text);
CREATE VIEW logv AS SELECT msg FROM log;
CREATE TABLE ruled(n int);
CREATE RULE ruled_copy AS ON INSERT TO ruled DO ALSO
  INSERT INTO log VALUES ('rule ' || NEW.n);
CREATE FUNCTION seen() RETURNS bigint LANGUAGE plpgsql
  AS $$BEGIN RETURN (SELECT count(*) FROM log); END$$;
CREATE FUNCTION bt() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE k bigint;
BEGIN
  k := nextval('s');
  RAISE NOTICE 'bt % % new % old %', TG_OP, k, NEW IS NULL, OLD IS NULL;
  IF TG_OP = 'DELETE' THEN
    INSERT INTO logv VALUES ('del ' || OLD.id || ' ' || k || ' ' || NEW.id IS NULL);
    IF OLD.id = 2 THEN RETURN NULL; END IF;
    RETURN OLD;
  END IF;
  EXECUTE 'CREATE TABLE IF NOT EXISTS made_' || NEW.id || '(x int)';
  INSERT INTO logv VALUES (TG_OP || ' ' || NEW.id || ' ' || k || ' ' || seen());
  INSERT INTO ruled VALUES (NEW.id);
  IF NEW.id = 3 THEN RETURN NULL; END IF;
  NEW.n := k;
  NEW.v := upper(NEW.v);
  RETURN NEW;
END$$;
CREATE TRIGGER bt BEFORE INSERT OR UPDATE OR DELETE ON t
  FOR EACH ROW EXECUTE FUNCTION bt();
INSERT INTO t(id, v) VALUES (1, 'a'), (2, 'b'), (3, 'c') RETURNING *, nextval('s') AS after;
INSERT INTO t(id, v) SELECT g, 'x' || g FROM generate_series(4, 5) g RETURNING id, v, n;
UPDATE t SET v = v || '!' WHERE id IN (1, 4) RETURNING id, v, n;
INSERT INTO t(id, v) VALUES (1, 'again') ON CONFLICT (id) DO UPDATE SET v = 'upd'
  RETURNING id, v, n;
MERGE INTO t USING (VALUES (5, 'm5'), (6, 'm6')) AS src(id, v) ON t.id = src.id
  WHEN MATCHED THEN UPDATE SET v = src.v
  WHEN NOT MATCHED THEN INSERT (id, v) VALUES (src.id, src.v)
  RETURNING merge_action(), t.id, t.v, t.n;
DELETE FROM t WHERE id IN (1, 2) RETURNING id, v;
SELECT * FROM t ORDER BY id;
SELECT * FROM log ORDER BY msg;
SELECT * FROM ruled ORDER BY n;
SELECT relname FROM pg_class
  WHERE relname LIKE 'made_%' AND relnamespace = 'executor_triggers'::regnamespace
  ORDER BY relname;
SELECT last_value FROM s;

-- A write that rolls back takes the trigger's DDL and view rows with it.
BEGIN;
INSERT INTO t(id, v) VALUES (20, 'r');
ROLLBACK;
SELECT count(*) FROM t WHERE id = 20;
SELECT count(*) FROM log WHERE msg LIKE 'INSERT 20 %';
SELECT count(*) FROM pg_class WHERE relname = 'made_20';

-- An error in the body undoes the whole statement.
CREATE FUNCTION fails() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO logv VALUES ('fails ' || NEW.id);
  IF NEW.id = 31 THEN RAISE EXCEPTION 'stop at %', NEW.id; END IF;
  RETURN NEW;
END$$;
CREATE TABLE e(id int);
CREATE TRIGGER fails BEFORE INSERT ON e FOR EACH ROW EXECUTE FUNCTION fails();
INSERT INTO e VALUES (30), (31);
SELECT count(*) FROM e;
SELECT count(*) FROM log WHERE msg LIKE 'fails%';

-- A procedure with DDL, called from a BEFORE trigger with NEW in its
-- arguments, and a handler naming any PostgreSQL condition.
CREATE PROCEDURE make(n int) LANGUAGE sql
  AS $$CREATE TABLE IF NOT EXISTS proc_made(x int); INSERT INTO log VALUES ('proc ' || n)$$;
CREATE FUNCTION calls() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  CALL make(NEW.id + 100);
  BEGIN
    EXECUTE 'CREATE TABLE once_made(x int)';
  EXCEPTION WHEN duplicate_table THEN RAISE NOTICE 'exists for %', NEW.id;
  END;
  RETURN NEW;
END$$;
CREATE TABLE c(id int);
CREATE TRIGGER calls BEFORE INSERT ON c FOR EACH ROW EXECUTE FUNCTION calls();
INSERT INTO c VALUES (1), (2);
SELECT msg FROM log WHERE msg LIKE 'proc%' ORDER BY msg;

-- Triggers of writes that other triggers make move with them.
CREATE TABLE a(n int);
CREATE TABLE b(n int);
CREATE FUNCTION ta() RETURNS trigger LANGUAGE plpgsql
  AS $$BEGIN INSERT INTO logv VALUES ('ta ' || NEW.n); INSERT INTO b VALUES (NEW.n); RETURN NEW; END$$;
CREATE FUNCTION tb() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  CREATE TABLE IF NOT EXISTS from_b(x int);
  INSERT INTO logv VALUES ('tb ' || NEW.n || ' ' || seen());
  RETURN NEW;
END$$;
CREATE TRIGGER ta BEFORE INSERT ON a FOR EACH ROW EXECUTE FUNCTION ta();
CREATE TRIGGER tb BEFORE INSERT ON b FOR EACH ROW EXECUTE FUNCTION tb();
INSERT INTO a VALUES (7), (8);
SELECT * FROM log WHERE msg LIKE 't_ %' ORDER BY msg;

-- NEW and OLD are NULL records in a statement trigger and where a row
-- trigger has no row; a field the table lacks is still an error.
CREATE TABLE nulls(id int);
CREATE FUNCTION st() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE NOTICE 'st % % %', NEW IS NULL, OLD.id, NEW; RETURN NULL; END$$;
CREATE TRIGGER st BEFORE INSERT ON nulls FOR EACH STATEMENT EXECUTE FUNCTION st();
INSERT INTO nulls VALUES (1);
CREATE FUNCTION nofield() RETURNS trigger LANGUAGE plpgsql
  AS $$BEGIN RAISE NOTICE '%', OLD.nope; RETURN NEW; END$$;
CREATE TRIGGER nofield BEFORE INSERT ON nulls FOR EACH ROW EXECUTE FUNCTION nofield();
INSERT INTO nulls VALUES (2);
CREATE FUNCTION badcondition() RETURNS int LANGUAGE plpgsql AS $$
BEGIN RETURN 1; EXCEPTION WHEN no_such_condition THEN RETURN 0; END$$;

-- Statement triggers, INSTEAD OF view triggers, partitions, and AFTER
-- triggers that call routines.
CREATE TABLE st_rows(id int);
CREATE FUNCTION st_ddl() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  EXECUTE 'CREATE TABLE IF NOT EXISTS st_made(x int)';
  INSERT INTO logv VALUES ('st ' || TG_WHEN || ' ' || TG_OP || ' ' || seen());
  RETURN NULL;
END$$;
CREATE TRIGGER st_before BEFORE INSERT ON st_rows FOR EACH STATEMENT EXECUTE FUNCTION st_ddl();
CREATE TRIGGER st_after AFTER INSERT ON st_rows FOR EACH STATEMENT EXECUTE FUNCTION st_ddl();
INSERT INTO st_rows VALUES (1);
CREATE VIEW st_view AS SELECT id FROM st_rows;
CREATE FUNCTION st_instead() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  EXECUTE 'CREATE TABLE IF NOT EXISTS instead_made(x int)';
  INSERT INTO st_rows VALUES (NEW.id * 10);
  RETURN NEW;
END$$;
CREATE TRIGGER st_instead INSTEAD OF INSERT ON st_view FOR EACH ROW EXECUTE FUNCTION st_instead();
INSERT INTO st_view VALUES (5), (6) RETURNING *;
CREATE TABLE parts(id int) PARTITION BY RANGE (id);
CREATE TABLE parts_low PARTITION OF parts FOR VALUES FROM (0) TO (100);
CREATE FUNCTION part_row() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  EXECUTE 'CREATE TABLE IF NOT EXISTS part_made(x int)';
  INSERT INTO logv VALUES ('part ' || TG_OP || ' ' || NEW.id);
  RETURN NEW;
END$$;
CREATE TRIGGER part_row BEFORE INSERT OR UPDATE ON parts FOR EACH ROW EXECUTE FUNCTION part_row();
INSERT INTO parts VALUES (1), (2);
UPDATE parts SET id = id + 10 RETURNING id;
CREATE FUNCTION after_call(x int) RETURNS int LANGUAGE plpgsql
  AS $$BEGIN INSERT INTO log VALUES ('after_call ' || x); RETURN x; END$$;
CREATE FUNCTION after_row() RETURNS trigger LANGUAGE plpgsql
  AS $$BEGIN PERFORM after_call(NEW.id); RETURN NULL; END$$;
CREATE TRIGGER after_row AFTER INSERT ON parts FOR EACH ROW EXECUTE FUNCTION after_row();
INSERT INTO parts VALUES (3), (4);
DO $$BEGIN INSERT INTO parts VALUES (5); END$$;
SELECT * FROM st_rows ORDER BY id;
SELECT * FROM parts ORDER BY id;
SELECT msg FROM log WHERE msg LIKE 'st %' OR msg LIKE 'part%' OR msg LIKE 'after%' ORDER BY msg;
DROP SCHEMA executor_triggers CASCADE;
