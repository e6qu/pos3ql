-- A trigger fired inside a writing statement reads a session cursor that
-- evaluates its rows as they are fetched, forward and scrolling.
CREATE TABLE src284 (a int);
INSERT INTO src284 SELECT generate_series(1, 6);
CREATE TABLE log284 (v int, note text);
CREATE TABLE tgt284 (a int, motion text);
CREATE FUNCTION fetch284() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE v int; c refcursor := 'sc284';
BEGIN
  IF NEW.motion = 'next' THEN
    FETCH c INTO v;
  ELSIF NEW.motion = 'prior' THEN
    FETCH PRIOR FROM c INTO v;
  ELSIF NEW.motion = 'absolute' THEN
    FETCH ABSOLUTE 2 FROM c INTO v;
  ELSE
    MOVE FORWARD 2 FROM c;
    v := -1;
  END IF;
  INSERT INTO log284 VALUES (v, NEW.motion || ' for ' || NEW.a);
  RETURN NEW;
END $$;
CREATE TRIGGER b284 BEFORE INSERT ON tgt284
  FOR EACH ROW EXECUTE FUNCTION fetch284();
BEGIN;
DECLARE sc284 NO SCROLL CURSOR FOR SELECT a * 10 FROM src284 WHERE a > 1;
INSERT INTO tgt284 VALUES (1, 'next'), (2, 'next'), (3, 'move');
FETCH NEXT FROM sc284;
SELECT * FROM log284 ORDER BY note;
ROLLBACK;
BEGIN;
DECLARE sc284 SCROLL CURSOR FOR SELECT a * 100 + (random() < 2)::int FROM src284;
INSERT INTO tgt284 VALUES (1, 'next'), (2, 'next'), (3, 'prior'), (4, 'absolute');
FETCH ALL FROM sc284;
FETCH BACKWARD 2 FROM sc284;
SELECT * FROM log284 ORDER BY note;
ROLLBACK;
BEGIN;
DECLARE sc284 CURSOR FOR SELECT a FROM src284 WHERE a % 2 = 0 OFFSET 1 LIMIT 2;
UPDATE src284 SET a = a WHERE a = 1;
INSERT INTO tgt284 VALUES (1, 'next'), (2, 'next'), (3, 'next');
SELECT * FROM log284 ORDER BY note;
ROLLBACK;
