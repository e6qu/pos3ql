-- Writes whose rows continue after a storage or lock wait keep PostgreSQL's
-- per-row order: sequence defaults, BEFORE and AFTER row triggers with their
-- own writes and sequence calls, transition tables, RETURNING rows, rule
-- steps, data-modifying WITH and MERGE. The waits themselves are exercised
-- by the engine tests; this fixture pins the values they must reproduce.
CREATE SCHEMA resumed_rows;
SET search_path = resumed_rows, public;
CREATE TABLE parents(id integer PRIMARY KEY);
INSERT INTO parents SELECT generate_series(1, 40);
CREATE SEQUENCE numbers;
CREATE TABLE audit(stage text, n bigint);
CREATE TABLE orders(n bigint DEFAULT nextval('numbers'),
  item integer REFERENCES parents, id integer UNIQUE, note text);
CREATE FUNCTION audit_order() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO audit VALUES (TG_OP || '-' || NEW.note, nextval('numbers'));
  NEW.note := NEW.note || '+';
  RETURN NEW;
END$$;
CREATE TRIGGER audit_order BEFORE INSERT OR UPDATE ON orders
  FOR EACH ROW EXECUTE FUNCTION audit_order();
CREATE FUNCTION after_order() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO audit VALUES ('after-' || NEW.note, nextval('numbers'));
  RETURN NULL;
END$$;
CREATE TRIGGER after_order AFTER INSERT OR UPDATE ON orders
  FOR EACH ROW EXECUTE FUNCTION after_order();
CREATE FUNCTION changed_orders() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO audit SELECT TG_OP || '-changed-' || count(*), nextval('numbers')
    FROM changed;
  RETURN NULL;
END$$;
CREATE TRIGGER changed_inserts AFTER INSERT ON orders
  REFERENCING NEW TABLE AS changed FOR EACH STATEMENT EXECUTE FUNCTION changed_orders();
CREATE TRIGGER changed_updates AFTER UPDATE ON orders
  REFERENCING NEW TABLE AS changed FOR EACH STATEMENT EXECUTE FUNCTION changed_orders();
INSERT INTO orders(item, id, note)
  SELECT CASE WHEN g >= 4 THEN g END, g, 'o' || g FROM generate_series(1, 6) g
  RETURNING n, item, id, note;
UPDATE orders SET item = CASE WHEN n > 3 THEN n + 1 END, id = id + 10, note = 'u'
  WHERE item IS NULL RETURNING n, item, id, note;
INSERT INTO orders(item, id, note)
  VALUES (NULL, 21, 'v1'), (NULL, 22, 'v2'), (8, 23, 'v3'), (9, 24, 'v4')
  RETURNING n, item, id, note;
SELECT stage, n FROM audit ORDER BY n;
SELECT count(*), sum(n), sum(id) FROM orders;

CREATE TABLE links(id int, more int REFERENCES parents, note text);
CREATE TABLE log(note text, more int REFERENCES parents);
CREATE TABLE gone_log(note text, more int REFERENCES parents);
CREATE TABLE pairs(id int, more int REFERENCES parents, note text);
CREATE RULE log_links AS ON INSERT TO links DO ALSO INSERT INTO log VALUES (NEW.note, NEW.id);
CREATE FUNCTION check_delete() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE seen bigint;
BEGIN
  IF OLD.id >= 3 THEN
    SELECT count(*) INTO seen FROM parents;
    INSERT INTO log VALUES ('deleting-' || OLD.id || '-' || seen, NULL);
  END IF;
  RETURN OLD;
END$$;
CREATE TRIGGER links_delete BEFORE DELETE ON links
  FOR EACH ROW EXECUTE FUNCTION check_delete();
INSERT INTO links SELECT g, CASE WHEN g >= 3 THEN g END, 'r' || g
  FROM generate_series(1, 4) g RETURNING id, more;
WITH gone AS (DELETE FROM links WHERE id <= 2 RETURNING id, note)
  INSERT INTO gone_log SELECT note || '-gone', id + 10 FROM gone RETURNING note, more;
INSERT INTO pairs VALUES (1, NULL, 'p1'), (2, NULL, 'p2');
MERGE INTO pairs p USING generate_series(1, 6) g ON p.id = g
  WHEN MATCHED THEN UPDATE SET more = g + 20
  WHEN NOT MATCHED THEN INSERT VALUES (g, g + 30, 'm' || g);
SELECT id, more, note FROM pairs ORDER BY id;
DELETE FROM links WHERE id >= 3 RETURNING id, note;
SELECT note, more FROM log ORDER BY note;
SELECT note, more FROM gone_log ORDER BY note;
SELECT count(*) FROM links;
DROP SCHEMA resumed_rows CASCADE;
