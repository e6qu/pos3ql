-- UPDATE, DELETE and MERGE data-modifying WITH queries return each row to
-- their reader before they change the next, as PostgreSQL's ModifyTable
-- does; a sort, an aggregate, a window or a join that hashes the query reads
-- it whole first. Row triggers and calls log the order.
CREATE SEQUENCE w414_seq;
CREATE TABLE w414_log (n int DEFAULT nextval('w414_seq'), msg text);
CREATE TABLE w414_a (x int);
CREATE TABLE w414_k (x int);
CREATE TABLE w414_out (x int);
CREATE FUNCTION w414_row() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO w414_log (msg) VALUES (TG_WHEN || ' ' || TG_OP || ' ' || TG_TABLE_NAME || ' ' || coalesce(NEW.x, OLD.x));
  IF TG_WHEN = 'BEFORE' AND coalesce(NEW.x, OLD.x) % 7 = 0 THEN RETURN NULL; END IF;
  RETURN coalesce(NEW, OLD);
END $$;
CREATE FUNCTION w414_note(t text, v int) RETURNS int LANGUAGE plpgsql VOLATILE AS $$
BEGIN
  INSERT INTO w414_log (msg) VALUES (t || ' ' || v);
  RETURN v;
END $$;
CREATE TRIGGER w414_a_before BEFORE INSERT OR UPDATE OR DELETE ON w414_a FOR EACH ROW EXECUTE FUNCTION w414_row();
CREATE TRIGGER w414_a_after AFTER INSERT OR UPDATE OR DELETE ON w414_a FOR EACH ROW EXECUTE FUNCTION w414_row();
CREATE TRIGGER w414_out_before BEFORE INSERT ON w414_out FOR EACH ROW EXECUTE FUNCTION w414_row();
INSERT INTO w414_a VALUES (1), (2), (3), (4);
INSERT INTO w414_k VALUES (2), (3), (5);
CREATE TRIGGER w414_k_before BEFORE UPDATE OR DELETE ON w414_k FOR EACH ROW EXECUTE FUNCTION w414_row();
-- A join input large enough that PostgreSQL hashes the query, whatever
-- statistics a background analyze leaves.
CREATE TABLE w414_big (x int);
INSERT INTO w414_big SELECT g FROM generate_series(1, 2000) g;
ANALYZE w414_big;
TRUNCATE w414_log;
-- An UPDATE whose BEFORE trigger skips a row (x + 10 = 14 is a multiple of 7)
WITH w AS (UPDATE w414_a SET x = x + 10 RETURNING x)
SELECT w414_note('main', x) FROM w;
SELECT msg FROM w414_log ORDER BY n; TRUNCATE w414_log;
-- A DELETE read only partly
WITH w AS (DELETE FROM w414_a WHERE x > 11 RETURNING x)
SELECT w414_note('main', x) FROM w LIMIT 1;
SELECT msg FROM w414_log ORDER BY n; TRUNCATE w414_log;
SELECT x FROM w414_a ORDER BY x;
DELETE FROM w414_a; INSERT INTO w414_a VALUES (1), (2), (3); TRUNCATE w414_log;
-- An UPDATE read by a write
WITH w AS (UPDATE w414_a SET x = x * 10 RETURNING x)
INSERT INTO w414_out SELECT w414_note('out', x) FROM w;
SELECT msg FROM w414_log ORDER BY n; TRUNCATE w414_log;
-- A chain: a DELETE read by an INSERT query read by the main query
WITH w1 AS (DELETE FROM w414_a WHERE x < 25 RETURNING x),
     w2 AS (INSERT INTO w414_out SELECT w414_note('w2', x) FROM w1 RETURNING x)
SELECT w414_note('main', x) FROM w2;
SELECT msg FROM w414_log ORDER BY n; TRUNCATE w414_log;
DELETE FROM w414_a; INSERT INTO w414_a VALUES (2), (3), (9); TRUNCATE w414_log;
-- MERGE with every action kind
WITH w AS (MERGE INTO w414_a t USING w414_k s ON t.x = s.x
           WHEN MATCHED AND t.x = 3 THEN DELETE
           WHEN MATCHED THEN UPDATE SET x = t.x + 100
           WHEN NOT MATCHED THEN INSERT VALUES (s.x)
           WHEN NOT MATCHED BY SOURCE THEN UPDATE SET x = t.x + 1000
           RETURNING merge_action(), t.x)
SELECT w414_note(merge_action, x) FROM w;
SELECT msg FROM w414_log ORDER BY n; TRUNCATE w414_log;
-- A sort reader with LIMIT
WITH w AS (INSERT INTO w414_a SELECT g FROM generate_series(5, 9) g RETURNING x)
SELECT w414_note('main', x) FROM w ORDER BY x DESC LIMIT 2;
SELECT msg FROM w414_log ORDER BY n; TRUNCATE w414_log;
-- An aggregate reader
WITH w AS (UPDATE w414_a SET x = x + 1 WHERE x < 10 RETURNING x)
SELECT w414_note('sum', sum(x)::int) FROM w;
SELECT msg FROM w414_log ORDER BY n; TRUNCATE w414_log;
-- A window reader
WITH w AS (INSERT INTO w414_a VALUES (61), (62) RETURNING x)
SELECT w414_note('win', (sum(x) OVER ())::int) FROM w;
SELECT msg FROM w414_log ORDER BY n; TRUNCATE w414_log;
-- A sort reads its whole input before it returns a row
WITH w AS (INSERT INTO w414_a SELECT g FROM generate_series(5, 7) g RETURNING x)
SELECT w414_note('main', x) FROM w ORDER BY x DESC;
SELECT msg FROM w414_log ORDER BY n; TRUNCATE w414_log;
-- A join that hashes the query reads it whole first
WITH w AS (INSERT INTO w414_a SELECT g FROM generate_series(2, 4) g RETURNING x)
SELECT w414_note('main', w.x) FROM w JOIN w414_big k ON k.x = w.x;
SELECT msg FROM w414_log ORDER BY n; TRUNCATE w414_log;
-- A reader without calls, of two queries
WITH w1 AS (INSERT INTO w414_a VALUES (31), (32) RETURNING x),
     w2 AS (UPDATE w414_k SET x = x RETURNING x)
SELECT x FROM w1 UNION ALL SELECT x FROM w2;
SELECT msg FROM w414_log ORDER BY n; TRUNCATE w414_log;
-- A reader without calls that reads one row
WITH w1 AS (INSERT INTO w414_a VALUES (41), (42), (43) RETURNING x)
SELECT x FROM w1 LIMIT 1;
SELECT msg FROM w414_log ORDER BY n; TRUNCATE w414_log;
-- A reader without calls that reads one row of the first of two queries:
-- the unread rows run after the main query, the last query first.
WITH w1 AS (INSERT INTO w414_a VALUES (71), (72), (73) RETURNING x),
     w2 AS (DELETE FROM w414_k WHERE x = 5 RETURNING x)
SELECT x FROM w1 LIMIT 1;
SELECT msg FROM w414_log ORDER BY n; TRUNCATE w414_log;
DROP TABLE w414_log, w414_a, w414_k, w414_out, w414_big;
DROP FUNCTION w414_row(), w414_note(text, int);
DROP SEQUENCE w414_seq;
