-- Statements made of several steps keep the steps they finished while their
-- expressions call routines: data-modifying WITH sub-statements, rule
-- actions and INSTEAD OF view triggers. Each call sees the changes made
-- before it and runs once.
CREATE SCHEMA step_progress;
SET search_path = step_progress, public;
CREATE TABLE items(id int PRIMARY KEY, a bigint);
INSERT INTO items VALUES (1, 10), (2, 20), (3, 30);
CREATE TABLE log(id int, v bigint);
CREATE FUNCTION total() RETURNS bigint LANGUAGE plpgsql
  AS $$BEGIN RETURN (SELECT sum(a) FROM items); END$$;
CREATE FUNCTION logged() RETURNS bigint LANGUAGE plpgsql
  AS $$BEGIN RETURN (SELECT count(*) FROM log); END$$;
CREATE FUNCTION twice(x bigint) RETURNS bigint LANGUAGE plpgsql
  AS $$BEGIN RETURN x * 2; END$$;

-- Data-modifying WITH: calls in a sub-statement and in the main statement.
WITH w AS (UPDATE items SET a = a + 1 RETURNING id, twice(a) AS t)
SELECT * FROM w ORDER BY id;
WITH w AS (UPDATE items SET a = total() RETURNING id, a)
SELECT * FROM w ORDER BY id;
WITH w AS (UPDATE items SET a = a + 1 RETURNING id, a)
INSERT INTO log SELECT id, twice(a) FROM w;
WITH w AS (INSERT INTO log VALUES (10, 0), (11, 0) RETURNING id)
SELECT id, logged() FROM w ORDER BY id;
WITH a AS (INSERT INTO log VALUES (20, 0) RETURNING id),
     b AS (INSERT INTO log SELECT id + 1, logged() FROM a RETURNING *)
SELECT * FROM b;
WITH m AS (MERGE INTO items t USING (VALUES (2), (4)) s(id) ON t.id = s.id
             WHEN MATCHED THEN UPDATE SET a = total()
             WHEN NOT MATCHED THEN INSERT VALUES (s.id, total())
             RETURNING merge_action(), t.*)
INSERT INTO log SELECT id, a FROM m;
SELECT * FROM items ORDER BY id;
SELECT * FROM log ORDER BY id, v;

-- Rules: each action is its own command and sees the earlier actions.
CREATE TABLE src(id int, v bigint);
CREATE RULE src_also AS ON UPDATE TO src DO ALSO
  INSERT INTO log VALUES (new.id + 100, total());
INSERT INTO src VALUES (1, 1), (2, 2);
UPDATE src SET v = twice(v);
SELECT * FROM src ORDER BY id;
CREATE TABLE src2(id int, v bigint);
CREATE RULE src2_instead AS ON INSERT TO src2 DO INSTEAD
  (INSERT INTO log VALUES (new.id, 1); INSERT INTO log VALUES (new.id, logged()));
INSERT INTO src2 VALUES (50, 0);
SELECT * FROM log ORDER BY id, v;

-- The command tag counts the original statement, or the last INSTEAD
-- action of the same command.
CREATE TABLE t1(id int);
CREATE TABLE t2(id int);
CREATE TABLE t3(id int);
CREATE RULE t1_instead AS ON INSERT TO t1 DO INSTEAD
  (INSERT INTO log VALUES (new.id, 1); UPDATE log SET v = v WHERE false;
   INSERT INTO log SELECT new.id, 2 FROM generate_series(1, 3));
INSERT INTO t1 VALUES (1), (2);
CREATE RULE t2_instead AS ON INSERT TO t2 DO INSTEAD UPDATE log SET v = v;
INSERT INTO t2 VALUES (1);
CREATE RULE t3_qualified AS ON INSERT TO t3 WHERE new.id > 1
  DO INSTEAD INSERT INTO log VALUES (new.id, 3);
CREATE RULE t3_also AS ON INSERT TO t3 DO ALSO INSERT INTO log VALUES (new.id, 4);
INSERT INTO t3 VALUES (1), (2), (3);
CREATE RULE t3_update AS ON UPDATE TO t3 DO INSTEAD INSERT INTO log VALUES (old.id, 5);
CREATE RULE t3_update_seen AS ON UPDATE TO t3 WHERE old.id > 0
  DO INSTEAD UPDATE log SET v = 6 WHERE id = old.id;
UPDATE t3 SET id = 9;
SELECT v, count(*) FROM log WHERE id = 1 GROUP BY v ORDER BY v;

-- Rules on a data-modifying WITH sub-statement must leave one statement.
CREATE TABLE r1(id int);
CREATE TABLE r2(id int);
CREATE TABLE r3(id int);
CREATE TABLE r4(id int);
CREATE TABLE r5(id int);
CREATE TABLE r6(id int);
CREATE RULE r1_also AS ON INSERT TO r1 DO ALSO INSERT INTO log VALUES (new.id, 0);
WITH w AS (INSERT INTO r1 VALUES (1) RETURNING id) SELECT * FROM w;
CREATE RULE r2_nothing AS ON INSERT TO r2 DO INSTEAD NOTHING;
WITH w AS (INSERT INTO r2 VALUES (1)) SELECT 1;
CREATE RULE r3_qualified AS ON INSERT TO r3 WHERE new.id > 0
  DO INSTEAD INSERT INTO log VALUES (new.id, 0);
WITH w AS (INSERT INTO r3 VALUES (1)) SELECT 1;
CREATE RULE r4_two AS ON INSERT TO r4 DO INSTEAD
  (INSERT INTO log VALUES (new.id, 0); INSERT INTO log VALUES (new.id, 0));
WITH w AS (INSERT INTO r4 VALUES (1)) SELECT 1;
CREATE RULE r5_notify AS ON INSERT TO r5 DO INSTEAD NOTIFY step_progress;
WITH w AS (INSERT INTO r5 VALUES (1)) SELECT 1;
CREATE RULE r6_instead AS ON INSERT TO r6 DO INSTEAD
  INSERT INTO log VALUES (new.id * 10, 7) RETURNING id;
WITH w AS (INSERT INTO r6 VALUES (1), (2) RETURNING id) SELECT * FROM w ORDER BY id;

-- INSTEAD OF view triggers: calls in row expressions and RETURNING.
CREATE TABLE stock(id int PRIMARY KEY, a bigint);
INSERT INTO stock VALUES (1, 1), (2, 2), (3, 3);
CREATE FUNCTION stocked() RETURNS bigint LANGUAGE plpgsql
  AS $$BEGIN RETURN (SELECT sum(a) FROM stock); END$$;
CREATE VIEW v AS SELECT id, a FROM stock;
CREATE FUNCTION v_write() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'INSERT' THEN
    INSERT INTO stock VALUES (new.id, new.a);
  ELSIF TG_OP = 'UPDATE' THEN
    UPDATE stock SET a = new.a WHERE id = old.id;
  ELSE
    DELETE FROM stock WHERE id = old.id;
    RETURN old;
  END IF;
  RETURN new;
END$$;
CREATE TRIGGER v_write INSTEAD OF INSERT OR UPDATE OR DELETE ON v
  FOR EACH ROW EXECUTE FUNCTION v_write();
INSERT INTO v VALUES (5, 1), (6, stocked()) RETURNING id, a, stocked();
UPDATE v SET a = stocked() WHERE id < 3 RETURNING id, a, stocked();
DELETE FROM v WHERE id > 4 RETURNING id, stocked();
SELECT * FROM stock ORDER BY id;

-- Writes from routines to views and ruled tables go through their rewriting.
CREATE VIEW plain AS SELECT id, a FROM stock;
DO $$BEGIN
  EXECUTE 'INSERT INTO plain VALUES ($1, $2)' USING 20, 2;
  EXECUTE 'INSERT INTO v VALUES (21, stocked())';
  EXECUTE 'INSERT INTO src VALUES (3, 3)';
  EXECUTE 'UPDATE src SET v = 0 WHERE id = 3';
END$$;
CREATE FUNCTION add_plain(x int) RETURNS int LANGUAGE sql
  AS $$INSERT INTO plain VALUES (x, x) RETURNING id$$;
SELECT add_plain(30);
SELECT * FROM stock ORDER BY id;
SELECT * FROM src ORDER BY id;
SELECT * FROM log WHERE id > 100 ORDER BY id, v;
DROP SCHEMA step_progress CASCADE;
