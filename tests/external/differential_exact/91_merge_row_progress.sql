-- MERGE keeps the rows it changed while its expressions call routines: each
-- call sees the rows changed before it, runs once, and statement triggers
-- see every changed row once.
CREATE SCHEMA merge_progress;
SET search_path = merge_progress, public;
CREATE TABLE items(id int, a bigint);
CREATE TABLE calls(label text, seen bigint);
CREATE FUNCTION seen(label text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE total bigint;
BEGIN
  SELECT count(*) + coalesce(sum(a), 0) INTO total FROM items;
  INSERT INTO calls VALUES (label, total);
  RETURN total;
END$$;
CREATE TABLE source(id int);
INSERT INTO source VALUES (1), (2), (3), (4), (5);
INSERT INTO items VALUES (1, 1), (2, 1), (3, 1), (6, 1);

MERGE INTO items t USING source s ON t.id = s.id
  WHEN MATCHED AND s.id = 3 THEN DELETE
  WHEN MATCHED THEN UPDATE SET a = seen('update') + 100
  WHEN NOT MATCHED THEN INSERT VALUES (s.id, seen('insert'))
  WHEN NOT MATCHED BY SOURCE THEN UPDATE SET a = seen('by source')
  RETURNING merge_action(), t.*, seen('returning');
SELECT * FROM items ORDER BY id;
SELECT label, seen FROM calls ORDER BY label, seen;
TRUNCATE calls;

-- Calls in the join condition run for each source and target pair.
BEGIN;
MERGE INTO items t USING source s ON t.id = s.id AND seen('on') > 0
  WHEN MATCHED THEN DELETE;
SELECT * FROM items ORDER BY id;
SELECT label, count(*) FROM calls GROUP BY label;
ROLLBACK;

-- Statement triggers see each operation's transition rows once.
CREATE TABLE trigger_log(label text, n bigint);
CREATE FUNCTION after_statement() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO trigger_log SELECT TG_OP || ' new', count(*) FROM new_rows;
  RETURN NULL;
END$$;
CREATE TRIGGER after_insert AFTER INSERT ON items
  REFERENCING NEW TABLE AS new_rows
  FOR EACH STATEMENT EXECUTE FUNCTION after_statement();
CREATE TRIGGER after_update AFTER UPDATE ON items
  REFERENCING NEW TABLE AS new_rows
  FOR EACH STATEMENT EXECUTE FUNCTION after_statement();
INSERT INTO source VALUES (7), (8);
MERGE INTO items t USING source s ON t.id = s.id
  WHEN MATCHED THEN UPDATE SET a = seen('update')
  WHEN NOT MATCHED THEN INSERT VALUES (s.id, seen('insert'));
SELECT * FROM trigger_log ORDER BY label;
SELECT * FROM items ORDER BY id;

-- An error after earlier rows changed rolls the whole MERGE back.
CREATE FUNCTION fail_at(x int) RETURNS bigint LANGUAGE plpgsql AS $$
BEGIN
  IF x = 4 THEN RAISE EXCEPTION 'stop at %', x; END IF;
  INSERT INTO calls VALUES ('fail_at', x);
  RETURN x;
END$$;
TRUNCATE calls;
MERGE INTO items t USING source s ON t.id = s.id
  WHEN MATCHED THEN UPDATE SET a = fail_at(s.id);
SELECT * FROM items ORDER BY id;
SELECT count(*) FROM calls;

DROP SCHEMA merge_progress CASCADE;
