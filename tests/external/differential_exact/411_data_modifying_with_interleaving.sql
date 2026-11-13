-- Data-modifying WITH queries run as their rows are read: each one's row
-- triggers and volatile calls interleave with the main query's reads, a
-- query read only partly still runs to completion, and queries nothing
-- reads run after the main query, last one first. The log pins the order.
CREATE SEQUENCE w411_seq;
CREATE TABLE w411_log (n int DEFAULT nextval('w411_seq'), msg text);
CREATE TABLE w411_a (x int);
CREATE TABLE w411_b (x int, y int);
CREATE TABLE w411_c (x int);
CREATE FUNCTION w411_row() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO w411_log (msg) VALUES (TG_WHEN || ' row ' || TG_TABLE_NAME || ' ' || NEW.x);
  RETURN NEW;
END $$;
CREATE FUNCTION w411_statement() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO w411_log (msg) VALUES (TG_WHEN || ' statement ' || TG_TABLE_NAME);
  RETURN NULL;
END $$;
CREATE FUNCTION w411_note(t text, v int) RETURNS int LANGUAGE plpgsql VOLATILE AS $$
BEGIN
  INSERT INTO w411_log (msg) VALUES (t || ' ' || v);
  RETURN v;
END $$;
CREATE TRIGGER w411_a_before BEFORE INSERT ON w411_a FOR EACH ROW EXECUTE FUNCTION w411_row();
CREATE TRIGGER w411_a_after AFTER INSERT ON w411_a FOR EACH ROW EXECUTE FUNCTION w411_row();
CREATE TRIGGER w411_a_statement AFTER INSERT ON w411_a FOR EACH STATEMENT EXECUTE FUNCTION w411_statement();
CREATE TRIGGER w411_b_before BEFORE INSERT ON w411_b FOR EACH ROW EXECUTE FUNCTION w411_row();
CREATE TRIGGER w411_b_after AFTER INSERT ON w411_b FOR EACH ROW EXECUTE FUNCTION w411_row();
CREATE TRIGGER w411_b_statement AFTER INSERT ON w411_b FOR EACH STATEMENT EXECUTE FUNCTION w411_statement();
CREATE TRIGGER w411_c_before BEFORE INSERT ON w411_c FOR EACH ROW EXECUTE FUNCTION w411_row();
CREATE TRIGGER w411_c_after AFTER INSERT ON w411_c FOR EACH ROW EXECUTE FUNCTION w411_row();
-- A chain of producers read by the main query, with an unread one.
WITH w1 AS (INSERT INTO w411_a SELECT g FROM generate_series(1, 3) g RETURNING x),
     w2 AS (INSERT INTO w411_b SELECT x, w411_note('b source', x) FROM w1 RETURNING x),
     w3 AS (INSERT INTO w411_c VALUES (100), (101) RETURNING x)
SELECT w411_note('main', x) FROM w2;
SELECT n, msg FROM w411_log ORDER BY n;
TRUNCATE w411_log, w411_a, w411_b, w411_c;
ALTER SEQUENCE w411_seq RESTART;
-- A producer read only partly still runs to completion.
WITH w1 AS (INSERT INTO w411_a SELECT g FROM generate_series(1, 4) g RETURNING x)
SELECT w411_note('main', x) FROM w1 LIMIT 2;
SELECT n, msg FROM w411_log ORDER BY n;
SELECT x FROM w411_a ORDER BY x;
TRUNCATE w411_log, w411_a, w411_b, w411_c;
ALTER SEQUENCE w411_seq RESTART;
-- Sequence values interleave between a producer and the write reading it.
CREATE SEQUENCE w411_values;
WITH w1 AS (INSERT INTO w411_a SELECT nextval('w411_values') FROM generate_series(1, 2) RETURNING x)
INSERT INTO w411_b SELECT x, nextval('w411_values') FROM w1;
SELECT x, y FROM w411_b ORDER BY x;
SELECT n, msg FROM w411_log ORDER BY n;
TRUNCATE w411_log, w411_a, w411_b, w411_c;
ALTER SEQUENCE w411_seq RESTART;
-- Producers read one after the other, and unread ones last one first.
WITH w1 AS (INSERT INTO w411_a VALUES (1), (2) RETURNING x),
     w2 AS (INSERT INTO w411_c VALUES (1), (2) RETURNING x),
     w3 AS (INSERT INTO w411_c VALUES (7) RETURNING x),
     w4 AS (INSERT INTO w411_a VALUES (8) RETURNING x)
SELECT w411_note('main a', x) FROM w1
UNION ALL
SELECT w411_note('main c', x) FROM w2;
SELECT n, msg FROM w411_log ORDER BY n;
DROP TABLE w411_log, w411_a, w411_b, w411_c;
DROP FUNCTION w411_row(), w411_statement(), w411_note(text, int);
DROP SEQUENCE w411_seq, w411_values;
