-- INSERT ... VALUES folds its constants before its statement triggers fire
-- and before any row is written: each VALUES row's constant conversion
-- raises its notice once, ahead of the rows' trigger notices, and a
-- RETURNING constant folds with the statement too.
CREATE TABLE c491 (x int, q tsquery);
CREATE FUNCTION c491_note() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE NOTICE '% %', TG_LEVEL, NEW.x; RETURN NEW; END $$;
CREATE TRIGGER c491_row BEFORE INSERT ON c491 FOR EACH ROW EXECUTE FUNCTION c491_note();
CREATE FUNCTION c491_statement() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE NOTICE 'statement'; RETURN NULL; END $$;
CREATE TRIGGER c491_statement BEFORE INSERT ON c491
  FOR EACH STATEMENT EXECUTE FUNCTION c491_statement();
INSERT INTO c491 VALUES (1, ''::tsquery), (2, ''::tsquery);
INSERT INTO c491 VALUES (3, ''::tsquery) RETURNING x, numnode(''::tsquery);
INSERT INTO c491 VALUES (4, 'a'::tsquery), (5, 'b'::tsquery)
  RETURNING x, numnode(''::tsquery);
INSERT INTO c491 VALUES (6, 'c'), (1 / 0, 'd');
SELECT x, q FROM c491 ORDER BY x;
