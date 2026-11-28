-- A row trigger's NEW and OLD hold their values as PostgreSQL's firing
-- does: a BEFORE trigger's NEW is the tuple heap_form_tuple forms of the
-- new row (a short value packed, a stored datum it passes on in its TOAST
-- form, any other value uncompressed), OLD and an AFTER trigger's NEW are
-- the rows as stored. A field assigned by an expression is held as the
-- expression formed it; one a query assigns as its result tuple holds it.
CREATE TABLE tf (id int, s text, big text);
CREATE TABLE src (big text);
INSERT INTO src VALUES (repeat('z', 3000));
CREATE FUNCTION tf_note() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r tf;
BEGIN
  RAISE NOTICE '% % new s % big %', TG_WHEN, TG_OP,
    pg_column_size(NEW.s), pg_column_size(NEW.big);
  IF TG_OP <> 'INSERT' THEN
    RAISE NOTICE '% % old s % big %', TG_WHEN, TG_OP,
      pg_column_size(OLD.s), pg_column_size(OLD.big);
  END IF;
  IF TG_WHEN = 'BEFORE' AND TG_OP <> 'DELETE' THEN
    r := NEW;
    RAISE NOTICE 'copied s % big %', pg_column_size(r.s), pg_column_size(r.big);
    NEW.s := upper(NEW.s);
    RAISE NOTICE 'assigned s %', pg_column_size(NEW.s);
    SELECT 'abc' INTO NEW.s;
    RAISE NOTICE 'selected s %', pg_column_size(NEW.s);
  END IF;
  IF TG_OP = 'DELETE' THEN
    RETURN OLD;
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER b BEFORE INSERT OR UPDATE OR DELETE ON tf
  FOR EACH ROW EXECUTE FUNCTION tf_note();
CREATE TRIGGER a AFTER INSERT OR UPDATE OR DELETE ON tf
  FOR EACH ROW EXECUTE FUNCTION tf_note();
INSERT INTO tf VALUES (1, 'short', repeat('a', 3000));
INSERT INTO tf SELECT 2, 'x', big FROM src;
INSERT INTO tf VALUES (3, 'mid', repeat('b', 200));
UPDATE tf SET id = id + 10 WHERE id = 1;
UPDATE tf SET big = big || '' WHERE id = 2;
DELETE FROM tf WHERE id = 3;
INSERT INTO tf VALUES (11, 'conflict', 'c') ON CONFLICT DO NOTHING;
SELECT id, pg_column_size(s), pg_column_size(big) FROM tf ORDER BY id;
-- A WHEN condition reads the rows as the firing holds them.
CREATE TABLE tw (s text, big text);
CREATE FUNCTION tw_note() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE NOTICE 'when passed %', TG_WHEN; RETURN NEW; END $$;
CREATE TRIGGER wb BEFORE INSERT ON tw FOR EACH ROW
  WHEN (pg_column_size(NEW.big) = 3004 AND pg_column_size(NEW.s) = 2)
  EXECUTE FUNCTION tw_note();
CREATE TRIGGER wa AFTER INSERT ON tw FOR EACH ROW
  WHEN (pg_column_size(NEW.big) < 100)
  EXECUTE FUNCTION tw_note();
INSERT INTO tw VALUES ('a', repeat('q', 3000));
-- A deferred constraint trigger reads its queued rows as stored.
CREATE TABLE tc (big text);
CREATE CONSTRAINT TRIGGER c AFTER INSERT OR UPDATE ON tc
  DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION tw_note();
CREATE FUNCTION tc_note() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE NOTICE 'deferred new %', pg_column_size(NEW.big); RETURN NEW; END $$;
CREATE CONSTRAINT TRIGGER d AFTER INSERT ON tc
  DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION tc_note();
BEGIN;
INSERT INTO tc VALUES (repeat('d', 3000));
INSERT INTO tc SELECT big FROM src;
COMMIT;
-- ON CONFLICT DO UPDATE and MERGE fire with the stored and written rows.
CREATE TABLE tu (id int PRIMARY KEY, big text);
CREATE FUNCTION tu_note() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'INSERT' THEN
    RAISE NOTICE '% % new %', TG_WHEN, TG_OP, pg_column_size(NEW.big);
  ELSE
    RAISE NOTICE '% % old % new %', TG_WHEN, TG_OP, pg_column_size(OLD.big),
      pg_column_size(NEW.big);
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER ub BEFORE INSERT OR UPDATE ON tu FOR EACH ROW EXECUTE FUNCTION tu_note();
CREATE TRIGGER ua AFTER INSERT OR UPDATE ON tu FOR EACH ROW EXECUTE FUNCTION tu_note();
INSERT INTO tu VALUES (1, repeat('e', 3000));
INSERT INTO tu VALUES (1, 'x') ON CONFLICT (id) DO UPDATE SET id = tu.id;
INSERT INTO tu VALUES (1, 'x') ON CONFLICT (id) DO UPDATE SET big = excluded.big;
MERGE INTO tu USING src ON tu.id = 1 WHEN MATCHED THEN UPDATE SET big = src.big || '';
MERGE INTO tu USING (VALUES (2)) v(k) ON tu.id = v.k
  WHEN NOT MATCHED THEN INSERT VALUES (v.k, repeat('f', 3000));
DROP TABLE tf, src, tw, tc, tu;
DROP FUNCTION tf_note, tw_note, tc_note, tu_note;
