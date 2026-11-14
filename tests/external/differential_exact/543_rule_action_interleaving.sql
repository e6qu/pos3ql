-- A rule action reads the triggering statement's expressions row by row:
-- PostgreSQL's rewriter substitutes them for each OLD and NEW reference, so
-- for each row the statement's expression runs where the action reads it,
-- interleaved with the action's own expressions. A statement's query
-- without volatile output is pulled up into the action the same way.
CREATE TABLE c543 (id int, a int);
CREATE TABLE c543_log (x int, y int);
INSERT INTO c543 VALUES (1, 10), (2, 20), (3, 30);
CREATE FUNCTION c543_f(label text, v int) RETURNS int LANGUAGE plpgsql AS $$
BEGIN RAISE NOTICE '% %', label, v; RETURN v; END $$;
CREATE RULE c543_update AS ON UPDATE TO c543
  DO ALSO INSERT INTO c543_log VALUES (new.a, c543_f('own', old.id));
UPDATE c543 SET a = c543_f('set', a) WHERE id < 3;
CREATE RULE c543_select AS ON UPDATE TO c543
  DO ALSO INSERT INTO c543_log SELECT old.id, c543_f('select', new.a);
UPDATE c543 SET a = c543_f('set2', a + 1);
DROP RULE c543_update ON c543;
DROP RULE c543_select ON c543;
CREATE RULE c543_insert AS ON INSERT TO c543
  DO ALSO INSERT INTO c543_log VALUES (new.id, c543_f('own', new.a));
INSERT INTO c543 SELECT g, g * 10 FROM generate_series(4, 5) g;
INSERT INTO c543 VALUES (6, c543_f('value', 60));
SELECT x, y FROM c543_log ORDER BY x, y;
