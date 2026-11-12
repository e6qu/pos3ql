-- Static PL/pgSQL INSERT, UPDATE and DELETE on views and on tables with
-- rules read the routine's variables and a trigger's NEW and OLD rows.
CREATE SCHEMA static_rewrite;
SET search_path = static_rewrite, public;
CREATE TABLE stock(id int PRIMARY KEY, a bigint);
CREATE VIEW plain AS SELECT id, a FROM stock;
CREATE TABLE log(id int, v text);
CREATE TABLE ruled(id int, v int);
CREATE RULE r AS ON INSERT TO ruled
  DO ALSO INSERT INTO log VALUES (new.id, 'ins ' || new.v);
CREATE RULE ru AS ON UPDATE TO ruled
  DO INSTEAD INSERT INTO log VALUES (old.id, 'upd ' || new.v);
CREATE FUNCTION stocked() RETURNS bigint LANGUAGE plpgsql
  AS $$BEGIN RETURN (SELECT coalesce(sum(a), 0) FROM stock); END$$;
CREATE FUNCTION w(x int, y bigint) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE r record; got bigint; n int;
BEGIN
  INSERT INTO plain VALUES (x, y) RETURNING a INTO got;
  RAISE NOTICE 'got % found %', got, found;
  GET DIAGNOSTICS n = ROW_COUNT;
  RAISE NOTICE 'rows %', n;
  UPDATE plain SET a = a + y WHERE id = x RETURNING * INTO r;
  RAISE NOTICE 'r %', r;
  INSERT INTO ruled VALUES (x, y);
  UPDATE ruled SET v = x * 10 WHERE id = x;
  GET DIAGNOSTICS n = ROW_COUNT;
  RAISE NOTICE 'rows %', n;
  DELETE FROM plain WHERE id = x AND a = got * 2;
  RAISE NOTICE 'found %', found;
  RETURN got;
END$$;
SELECT w(1, 5);
SELECT w(2, 7);
SELECT * FROM stock ORDER BY id;
SELECT * FROM ruled ORDER BY id;
SELECT * FROM log ORDER BY id, v;

-- Routine calls in the rows of a static write to a view keep its rows.
DO $$DECLARE base int := 10; total bigint;
BEGIN
  INSERT INTO plain VALUES (base + 1, 1), (base + 2, stocked());
  INSERT INTO plain VALUES (base + 3, stocked()) RETURNING stocked() INTO total;
  RAISE NOTICE 'total %', total;
  UPDATE plain SET a = stocked() WHERE id > base;
END$$;
SELECT * FROM stock ORDER BY id;

-- A variable and a column of the same name are ambiguous.
CREATE FUNCTION amb(id int) RETURNS void LANGUAGE plpgsql AS $$
BEGIN INSERT INTO plain SELECT id, 1 FROM stock; END$$;
SELECT amb(3);
-- INTO needs RETURNING; RETURNING needs INTO.
DO $$DECLARE v int; BEGIN INSERT INTO plain VALUES (40, 1) INTO v; END$$;
DO $$BEGIN INSERT INTO plain VALUES (41, 1) RETURNING id; END$$;

-- Trigger functions read NEW and OLD, field by field and whole.
CREATE TABLE base(id int, v int);
CREATE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO plain VALUES (new.id + 100, new.v);
  INSERT INTO ruled SELECT (new).id + 200, new.v;
  UPDATE plain SET a = a + new.v WHERE id = new.id + 100;
  RETURN new;
END$$;
CREATE TRIGGER t AFTER INSERT ON base FOR EACH ROW EXECUTE FUNCTION trg();
INSERT INTO base VALUES (7, 70);
CREATE FUNCTION trg_del() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN DELETE FROM plain WHERE id = old.id + 100; RETURN old; END$$;
CREATE TRIGGER td AFTER DELETE ON base FOR EACH ROW EXECUTE FUNCTION trg_del();
DELETE FROM base;
SELECT * FROM stock ORDER BY id;
SELECT * FROM log ORDER BY id, v;
DROP SCHEMA static_rewrite CASCADE;
