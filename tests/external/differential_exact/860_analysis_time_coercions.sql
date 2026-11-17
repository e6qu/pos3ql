-- An INSERT ... SELECT, ON CONFLICT or MERGE assignment of a constant folds
-- its length coercion while the statement is planned, so it fails before any
-- row is read; an unknown or non-collatable COLLATE fails at analysis.
CREATE TABLE ac846 (id integer PRIMARY KEY, s varchar(3), n numeric(3,1), c char(2));
CREATE TABLE ac846_src (id integer, s text);
-- Empty source: PostgreSQL still fails the constant at plan time.
INSERT INTO ac846 (id, s) SELECT id, 'abcdef' FROM ac846_src;
INSERT INTO ac846 (id, n) SELECT id, 123.45 FROM ac846_src;
INSERT INTO ac846 (id, c) SELECT id, 'xyz' FROM ac846_src;
INSERT INTO ac846 (id, s) SELECT 1, 'abcdef' WHERE false;
INSERT INTO ac846 (id, s) VALUES (1, 'abc');
INSERT INTO ac846 (id, s) VALUES (1, 'abc')
  ON CONFLICT (id) DO UPDATE SET s = 'abcdef';
INSERT INTO ac846 (id, s) VALUES (2, 'abc')
  ON CONFLICT (id) DO UPDATE SET n = 123.45;
INSERT INTO ac846 (id, s) VALUES (3, 'abc')
  ON CONFLICT (id) DO UPDATE SET s = 'abcdef' WHERE false;
MERGE INTO ac846 t USING ac846_src s ON t.id = s.id
  WHEN MATCHED THEN UPDATE SET s = 'abcdef';
MERGE INTO ac846 t USING ac846_src s ON t.id = s.id
  WHEN NOT MATCHED THEN INSERT (id, s) VALUES (s.id, 'abcdef');
MERGE INTO ac846 t USING ac846_src s ON t.id = s.id
  WHEN MATCHED AND false THEN UPDATE SET n = 123.45;
UPDATE ac846 SET s = 'abcdef' WHERE false;
UPDATE ac846 SET n = 123.45, c = 'xyz' WHERE id = -1;
INSERT INTO ac846 (id, s) VALUES (7, 'abc'), (8, 'abcdef') ON CONFLICT DO NOTHING;
SELECT * FROM ac846;
-- Collations.
SELECT 'a' COLLATE "nosuch846";
SELECT 'a' COLLATE nosuch846.x;
SELECT 1 COLLATE "C";
SELECT 1 COLLATE "nosuch846";
SELECT id COLLATE "C" FROM ac846_src;
SELECT id COLLATE "nosuch846" FROM ac846_src WHERE false;
SELECT s FROM ac846_src ORDER BY s COLLATE "nosuch846";
SELECT s FROM ac846_src ORDER BY id COLLATE "C";
INSERT INTO ac846 (id, s) SELECT id, s COLLATE "nosuch846" FROM ac846_src;
INSERT INTO ac846 (id) SELECT id COLLATE "C" FROM ac846_src;
UPDATE ac846 SET s = 'a' COLLATE "nosuch846" WHERE false;
UPDATE ac846 SET id = 2 COLLATE "C" WHERE false;
DELETE FROM ac846 WHERE id COLLATE "C" = 1 AND false;
CREATE TABLE ac846_bad (a integer COLLATE "C");
CREATE TABLE ac846_bad (a text COLLATE "nosuch846");
SELECT 'a'::text COLLATE "C" = 'b' COLLATE "nosuch846";
