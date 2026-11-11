-- Routine calls inside a write's rows see the rows the write already
-- changed, run once, and keep the write's triggers, transition tables,
-- sequences and RETURNING rows: at top level, in DO bodies, in dynamic
-- EXECUTE and in SQL-language functions.
CREATE SCHEMA row_progress;
SET search_path = row_progress, public;
CREATE TABLE items(id int, a bigint);
CREATE TABLE calls(label text, seen bigint);
CREATE SEQUENCE call_numbers;
CREATE FUNCTION seen(label text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE total bigint;
BEGIN
  SELECT coalesce(sum(a), 0) + count(*) INTO total FROM items;
  INSERT INTO calls VALUES (label, total);
  RETURN total;
END$$;
CREATE FUNCTION plain(x int) RETURNS int LANGUAGE plpgsql
  AS $$BEGIN INSERT INTO calls VALUES ('plain', x); RETURN x * 10; END$$;

INSERT INTO items VALUES (1, 1), (2, 1), (3, 1);
UPDATE items SET a = seen('update') RETURNING id, a;
DELETE FROM items WHERE id = 3 RETURNING id, seen('delete returning');
INSERT INTO items VALUES (4, seen('insert')), (5, seen('insert'))
  RETURNING id, a, seen('insert returning');
SELECT * FROM items ORDER BY id;
SELECT label, seen FROM calls ORDER BY label, seen;
TRUNCATE calls;

-- Procedural writes, static and dynamic, call PL/pgSQL functions.
DO $$BEGIN
  INSERT INTO items VALUES (plain(6), 0);
  UPDATE items SET a = seen('do update') WHERE id < 3;
  EXECUTE 'UPDATE items SET a = seen($1) WHERE id > 3' USING 'execute update';
  DELETE FROM items WHERE a = plain(0) / 10;
END$$;
SELECT * FROM items ORDER BY id;
SELECT label, seen FROM calls ORDER BY label, seen;
TRUNCATE calls;

-- SQL-language functions keep their returned rows.
CREATE FUNCTION bump() RETURNS SETOF bigint LANGUAGE sql
  AS $$UPDATE items SET a = a + 1 RETURNING seen('sql returning')$$;
SELECT bump();
SELECT label, seen FROM calls ORDER BY label, seen;
TRUNCATE calls;

-- Row triggers call routines; statement triggers see every changed row once.
CREATE TABLE trigger_log(label text, n bigint);
CREATE FUNCTION before_row() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN NEW.a := NEW.a + seen('before row'); RETURN NEW; END$$;
CREATE FUNCTION after_statement() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO trigger_log SELECT 'changed', count(*) FROM changed;
  INSERT INTO trigger_log VALUES ('sequence', nextval('call_numbers'));
  RETURN NULL;
END$$;
CREATE TRIGGER before_row BEFORE UPDATE ON items
  FOR EACH ROW EXECUTE FUNCTION before_row();
CREATE TRIGGER after_statement AFTER UPDATE ON items
  REFERENCING NEW TABLE AS changed
  FOR EACH STATEMENT EXECUTE FUNCTION after_statement();
UPDATE items SET a = nextval('call_numbers') RETURNING id, a;
SELECT * FROM trigger_log ORDER BY label;
SELECT label, seen FROM calls ORDER BY label, seen;
DROP TRIGGER before_row ON items;
DROP TRIGGER after_statement ON items;

-- ON CONFLICT updates call routines that see earlier rows.
CREATE TABLE keyed(id int PRIMARY KEY, a bigint);
INSERT INTO keyed VALUES (1, 0);
CREATE FUNCTION keyed_count() RETURNS bigint LANGUAGE plpgsql
  AS $$BEGIN RETURN (SELECT count(*) FROM keyed); END$$;
INSERT INTO keyed VALUES (1, 5), (2, 5), (3, 5)
  ON CONFLICT (id) DO UPDATE SET a = keyed_count()
  RETURNING id, a, keyed_count();

-- An error after earlier rows changed rolls the whole statement back.
CREATE FUNCTION fail_at(x int) RETURNS int LANGUAGE plpgsql AS $$
BEGIN
  IF x = 2 THEN RAISE EXCEPTION 'stop at %', x; END IF;
  INSERT INTO calls VALUES ('fail_at', x);
  RETURN x;
END$$;
TRUNCATE calls;
BEGIN;
UPDATE keyed SET a = fail_at(id);
ROLLBACK;
SELECT * FROM keyed ORDER BY id;
SELECT count(*) FROM calls;
DO $$BEGIN
  BEGIN
    UPDATE keyed SET a = fail_at(id);
  EXCEPTION WHEN raise_exception THEN
    INSERT INTO calls VALUES ('caught', 0);
  END;
END$$;
SELECT * FROM keyed ORDER BY id;
SELECT label, seen FROM calls ORDER BY label, seen;

DROP SCHEMA row_progress CASCADE;
