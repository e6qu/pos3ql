-- ALTER TABLE ... ALTER COLUMN: SET/DROP DEFAULT and SET/DROP NOT NULL.
-- Catalog-level column changes that persist and are enforced on later DML,
-- matching PostgreSQL 18 (the COLUMN keyword is optional, as in PostgreSQL).
DROP TABLE IF EXISTS ac;
CREATE TABLE ac (id int, a int, b text);
INSERT INTO ac VALUES (1, 10, 'x'), (2, NULL, 'y');

-- SET DEFAULT applies to columns omitted from a later INSERT; existing rows
-- are untouched. The default is cast to the column type.
ALTER TABLE ac ALTER COLUMN b SET DEFAULT 'dflt';
ALTER TABLE ac ALTER a SET DEFAULT 99;
INSERT INTO ac (id) VALUES (3);
SELECT id, a, b FROM ac ORDER BY id;

-- DROP DEFAULT: an omitted column falls back to NULL again.
ALTER TABLE ac ALTER COLUMN a DROP DEFAULT;
INSERT INTO ac (id) VALUES (4);
SELECT id, a FROM ac WHERE id = 4;

-- SET NOT NULL is refused while a NULL is present (23502), naming the column
-- and relation; it succeeds once the data is clean and is then enforced.
ALTER TABLE ac ALTER COLUMN a SET NOT NULL;
UPDATE ac SET a = 0 WHERE a IS NULL;
ALTER TABLE ac ALTER COLUMN a SET NOT NULL;
INSERT INTO ac (id, b) VALUES (5, 'z');

-- DROP NOT NULL lifts the constraint; a NULL then inserts.
ALTER TABLE ac ALTER COLUMN a DROP NOT NULL;
INSERT INTO ac (id, b) VALUES (6, 'w');
SELECT id, a FROM ac ORDER BY id;

-- A default that needs a cast (text literal into an int column).
ALTER TABLE ac ALTER COLUMN a SET DEFAULT '-7';
INSERT INTO ac (id) VALUES (7);
SELECT a, pg_typeof(a) FROM ac WHERE id = 7;

-- Errors: unknown column (42703); DROP NOT NULL on a primary-key column
-- (42P16).
ALTER TABLE ac ALTER COLUMN nope SET NOT NULL;
DROP TABLE IF EXISTS pk;
CREATE TABLE pk (id int PRIMARY KEY, v text);
ALTER TABLE pk ALTER COLUMN id DROP NOT NULL;

DROP TABLE ac;
DROP TABLE pk;
