-- A row an UPDATE ... FROM or a MERGE writes holds a source row's datum in
-- its stored form: a BEFORE trigger's NEW sees it compressed (44) or as its
-- TOAST pointer (3000), not formed anew (3004).
CREATE TABLE src (k int, big text, ext text);
ALTER TABLE src ALTER ext SET STORAGE EXTERNAL;
INSERT INTO src VALUES (1, repeat('z', 3000), repeat('e', 3000)),
  (2, repeat('y', 3000), repeat('f', 3000));
CREATE TABLE tu (id int PRIMARY KEY, big text, ext text);
INSERT INTO tu VALUES (1, 'x', 'x'), (2, 'x', 'x');
CREATE FUNCTION tu_note() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE NOTICE '% % new % %', TG_WHEN, TG_OP, pg_column_size(NEW.big),
    pg_column_size(NEW.ext);
  RETURN NEW;
END $$;
CREATE TRIGGER ub BEFORE INSERT OR UPDATE ON tu FOR EACH ROW EXECUTE FUNCTION tu_note();
CREATE TRIGGER ua AFTER INSERT OR UPDATE ON tu FOR EACH ROW EXECUTE FUNCTION tu_note();
UPDATE tu SET big = src.big, ext = src.ext FROM src WHERE tu.id = src.k AND src.k = 1;
MERGE INTO tu USING src ON tu.id = src.k
  WHEN MATCHED AND src.k = 2 THEN UPDATE SET big = src.big, ext = src.ext
  WHEN MATCHED THEN UPDATE SET big = upper(src.big)
  WHEN NOT MATCHED THEN INSERT VALUES (src.k, src.big, src.ext);
MERGE INTO tu USING (SELECT k + 2 AS k, big, ext FROM src) s ON tu.id = s.k
  WHEN NOT MATCHED THEN INSERT VALUES (s.k, s.big, s.ext);
MERGE INTO tu USING src ON tu.id = src.k + 2
  WHEN MATCHED THEN UPDATE SET big = src.big, ext = src.ext;
SELECT id, pg_column_size(big), pg_column_size(ext) FROM tu ORDER BY id;
-- A PL/pgSQL variable holding a stored datum passes it on.
DO $$
DECLARE v text;
BEGIN
  SELECT big INTO v FROM src WHERE k = 1;
  UPDATE tu SET big = v WHERE id = 1;
END $$;
-- An INSTEAD OF trigger's OLD is the view's row flattened, every value
-- formed anew; NEW holds the written row, a source datum in its stored
-- form and an unassigned value as OLD holds it.
CREATE TABLE base (id int, s text, big text, ext text);
ALTER TABLE base ALTER ext SET STORAGE EXTERNAL;
INSERT INTO base VALUES (1, 'ab', repeat('q', 3000), repeat('r', 3000));
CREATE VIEW v AS SELECT * FROM base;
CREATE FUNCTION v_note() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP <> 'INSERT' THEN
    RAISE NOTICE '% old % % %', TG_OP, pg_column_size(OLD.s),
      pg_column_size(OLD.big), pg_column_size(OLD.ext);
  END IF;
  IF TG_OP <> 'DELETE' THEN
    RAISE NOTICE '% new % % %', TG_OP, pg_column_size(NEW.s),
      pg_column_size(NEW.big), pg_column_size(NEW.ext);
    RETURN NEW;
  END IF;
  RETURN OLD;
END $$;
CREATE TRIGGER vi INSTEAD OF INSERT OR UPDATE OR DELETE ON v
  FOR EACH ROW EXECUTE FUNCTION v_note();
INSERT INTO v VALUES (2, 'cd', repeat('a', 3000), repeat('b', 3000));
INSERT INTO v SELECT k, 'ef', big, ext FROM src WHERE k = 1;
UPDATE v SET s = 'gh';
UPDATE v SET big = src.big FROM src WHERE src.k = 1;
DELETE FROM v;
MERGE INTO v USING src ON v.id = src.k
  WHEN MATCHED THEN UPDATE SET ext = src.ext
  WHEN NOT MATCHED THEN INSERT VALUES (src.k, 'ij', src.big, src.ext);
-- A row an UPDATE moves to another partition passes its stored datums on.
CREATE TABLE pt (id int, s text, big text, ext text) PARTITION BY RANGE (id);
CREATE TABLE p1 PARTITION OF pt FOR VALUES FROM (0) TO (10);
CREATE TABLE p2 PARTITION OF pt FOR VALUES FROM (10) TO (20);
ALTER TABLE p2 ALTER ext SET STORAGE EXTERNAL;
INSERT INTO pt VALUES (1, 'ab', repeat('q', 3000), repeat('r', 3000)),
  (2, 'cd', repeat('q', 3000), repeat('r', 3000));
CREATE FUNCTION p_note() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP <> 'INSERT' THEN
    RAISE NOTICE '% % % old % % %', TG_WHEN, TG_OP, TG_TABLE_NAME,
      pg_column_size(OLD.s), pg_column_size(OLD.big), pg_column_size(OLD.ext);
  END IF;
  IF TG_OP <> 'DELETE' THEN
    RAISE NOTICE '% % % new % % %', TG_WHEN, TG_OP, TG_TABLE_NAME,
      pg_column_size(NEW.s), pg_column_size(NEW.big), pg_column_size(NEW.ext);
    RETURN NEW;
  END IF;
  RETURN OLD;
END $$;
CREATE TRIGGER b BEFORE INSERT OR UPDATE OR DELETE ON pt FOR EACH ROW EXECUTE FUNCTION p_note();
CREATE TRIGGER a AFTER INSERT OR UPDATE OR DELETE ON pt FOR EACH ROW EXECUTE FUNCTION p_note();
UPDATE pt SET id = id + 10 WHERE id = 1;
UPDATE pt SET id = id + 10, big = src.big FROM src WHERE pt.id = 2 AND src.k = 1;
MERGE INTO pt USING src ON pt.id = src.k + 10
  WHEN MATCHED THEN UPDATE SET id = pt.id - 10, ext = src.ext;
SELECT id, pg_column_size(s), pg_column_size(big), pg_column_size(ext) FROM pt ORDER BY id;
DROP VIEW v;
DROP TABLE src, tu, base, pt;
DROP FUNCTION tu_note, v_note, p_note;
