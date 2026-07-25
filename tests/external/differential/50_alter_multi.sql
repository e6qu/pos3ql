-- ALTER TABLE with a comma-separated subcommand list. PostgreSQL applies the
-- list in a fixed pass order (drops, then type changes, then column adds, then
-- constraint adds, then column-attribute changes) rather than written order,
-- and the whole statement is atomic — a failure leaves the table untouched.
-- Matches PostgreSQL 18.
DROP TABLE IF EXISTS m;
CREATE TABLE m (a int);
INSERT INTO m VALUES (1), (2), (3);

-- Several ADD COLUMNs in one statement.
ALTER TABLE m ADD COLUMN b int DEFAULT 10, ADD COLUMN c text DEFAULT 'x';
SELECT a, b, c FROM m ORDER BY a;

-- An ADD CONSTRAINT written before the ADD COLUMN it references still works:
-- ADD COLUMN is an earlier pass, so column d exists by the time the CHECK is
-- built.
ALTER TABLE m ADD CONSTRAINT dpos CHECK (d > 0), ADD COLUMN d int DEFAULT 1;
SELECT a, d FROM m ORDER BY a;

-- A type change composed with SET NOT NULL, the latter validated against the
-- rewritten image.
ALTER TABLE m ALTER COLUMN a TYPE bigint, ALTER COLUMN a SET NOT NULL;
SELECT a FROM m ORDER BY a;

-- A uniqueness constraint added alongside a constant-default column collides on
-- every row (23505), and the whole statement rolls back: column u never exists.
ALTER TABLE m ADD COLUMN u int DEFAULT 7, ADD UNIQUE (u);
SELECT u FROM m;

-- A mid-list error is atomic: the ADD before the failing DROP does not apply
-- (42703 for the missing column, then 42703 again for the un-added g).
ALTER TABLE m ADD COLUMN g int, DROP COLUMN nope;
SELECT g FROM m;

-- DROP one column and ADD another in one statement.
ALTER TABLE m DROP COLUMN c, ADD COLUMN h int DEFAULT 99;
SELECT a, b, d, h FROM m ORDER BY a;

DROP TABLE m;
