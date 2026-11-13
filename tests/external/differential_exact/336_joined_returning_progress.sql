-- A routine call in the RETURNING list of UPDATE ... FROM and DELETE ...
-- USING runs after its row changed; the row and the FROM row it joined are
-- kept, so every row, trigger and returned row happens once.
CREATE TABLE jr336 (a int, b text);
CREATE TABLE jr336_source (a int, c text);
CREATE FUNCTION jr336_note(v text) RETURNS text LANGUAGE plpgsql AS $$
BEGIN RAISE NOTICE 'note %', v; RETURN v || '!'; END $$;
CREATE FUNCTION jr336_log() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE NOTICE '% % % %', TG_TABLE_NAME, TG_WHEN, TG_OP, coalesce(NEW, OLD)::text;
  RETURN coalesce(NEW, OLD);
END $$;
CREATE TRIGGER a AFTER UPDATE OR DELETE ON jr336 FOR EACH ROW EXECUTE FUNCTION jr336_log();
INSERT INTO jr336 VALUES (1, 'x'), (2, 'y'), (3, 'z');
INSERT INTO jr336_source VALUES (1, 'p'), (2, 'q');
UPDATE jr336 SET b = jr336_note(s.c) FROM jr336_source s WHERE jr336.a = s.a
  RETURNING jr336.a, jr336.b, jr336_note(s.c || jr336.b), s.*;
UPDATE jr336 SET b = 'n' FROM jr336_source s WHERE jr336.a = s.a
  RETURNING old.b, new.b, jr336_note(s.c);
DELETE FROM jr336 USING jr336_source s WHERE jr336.a = s.a
  RETURNING jr336.a, jr336_note(s.c), s.c;
SELECT * FROM jr336 ORDER BY a;
DROP TABLE jr336, jr336_source;
DROP FUNCTION jr336_note(text), jr336_log();
