-- A rule action reads the triggering statement's expression for each OLD or
-- NEW reference it makes, as PostgreSQL's rewriter substitutes them: an
-- action evaluates only the SET expressions it reads, once per reference,
-- so an unread SET expression neither raises its notice nor its error.
CREATE TABLE c510 (id serial, a int, b int);
CREATE TABLE c510_log (x int, y int);
INSERT INTO c510 (a, b) VALUES (10, 20), (30, 40);
CREATE FUNCTION c510_f(label text, v int) RETURNS int LANGUAGE plpgsql AS $$
BEGIN RAISE NOTICE '% %', label, v; RETURN v; END $$;
CREATE RULE c510_also AS ON UPDATE TO c510
  DO ALSO INSERT INTO c510_log VALUES (new.a, new.a + 1);
UPDATE c510 SET a = c510_f('a', a), b = c510_f('b', b) WHERE id = 1;
CREATE RULE c510_when AS ON UPDATE TO c510 WHERE new.b > 0
  DO ALSO INSERT INTO c510_log VALUES (old.id, 0);
UPDATE c510 SET a = c510_f('a2', a), b = c510_f('b2', b) WHERE id = 2;
DROP RULE c510_also ON c510;
DROP RULE c510_when ON c510;
-- An INSTEAD action that never reads b leaves its division unevaluated.
CREATE RULE c510_instead AS ON UPDATE TO c510
  DO INSTEAD INSERT INTO c510_log VALUES (new.a, old.b);
UPDATE c510 SET a = a + 100, b = b / 0;
SELECT x, y FROM c510_log ORDER BY x, y;
SELECT id, a, b FROM c510 ORDER BY id;
-- One VALUES row substitutes its expressions too, defaults included, so
-- each NEW.id reference draws its own sequence value.
CREATE RULE c510_insert AS ON INSERT TO c510
  DO ALSO INSERT INTO c510_log VALUES (new.id, new.id);
INSERT INTO c510 (a, b) VALUES (c510_f('ia', 1), c510_f('ib', 2));
CREATE RULE c510_insert_b AS ON INSERT TO c510
  DO ALSO INSERT INTO c510_log VALUES (new.b, c510_f('own', 0));
INSERT INTO c510 (a, b) VALUES (c510_f('ja', 3), c510_f('jb', 4));
-- A query's or several rows' NEW columns are the query's outputs, each
-- evaluated once per action.
INSERT INTO c510 (a, b) SELECT c510_f('sa', 5), c510_f('sb', 6);
SELECT x, y FROM c510_log ORDER BY x, y;
DELETE FROM c510_log;
CREATE RULE c510_delete AS ON DELETE TO c510
  DO ALSO INSERT INTO c510_log VALUES (old.a, old.a);
DELETE FROM c510 WHERE id = 1;
SELECT x, y FROM c510_log ORDER BY x, y;
