-- Explicit indexes: the pg_indexes view, pg_get_indexdef shapes, and — the
-- part a padding bug once silently broke — UNIQUE index enforcement over
-- exactly the written columns, no more and no fewer.
DROP TABLE IF EXISTS ix;
CREATE TABLE ix (a int PRIMARY KEY, b text, c int);
CREATE INDEX ix_bc ON ix (b, c);
CREATE UNIQUE INDEX ix_b ON ix (b);
SELECT schemaname, tablename, indexname, indexdef FROM pg_indexes WHERE tablename = 'ix' ORDER BY indexname;

-- A UNIQUE index on (b) alone: a duplicate b must collide even when every
-- other column differs.
INSERT INTO ix VALUES (1, 'x', 10);
INSERT INTO ix VALUES (2, 'x', 20);
INSERT INTO ix VALUES (3, 'y', 10);
SELECT a, b, c FROM ix ORDER BY a;

-- Multi-column UNIQUE: the pair must repeat to collide.
DROP TABLE IF EXISTS ix2;
CREATE TABLE ix2 (p int, q int);
CREATE UNIQUE INDEX ix2_pq ON ix2 (p, q);
INSERT INTO ix2 VALUES (1, 1), (1, 2), (2, 1);
INSERT INTO ix2 VALUES (1, 2);
SELECT count(*) FROM ix2;

DROP TABLE ix;
DROP TABLE ix2;

-- Index creation must validate this transaction's inserts and replacements,
-- not just the committed values that preceded them.
CREATE TABLE index_validation (id integer, label text, active boolean);
INSERT INTO index_validation VALUES (1, 'one', true), (2, 'two', false);
BEGIN;
SAVEPOINT before_validation;
INSERT INTO index_validation VALUES (3, 'fresh', true), (4, 'fresh', true);
CREATE UNIQUE INDEX index_validation_key ON index_validation(label);
ROLLBACK TO SAVEPOINT before_validation;
SELECT count(*) FROM pg_indexes WHERE indexname = 'index_validation_key';
UPDATE index_validation SET label = 'one' WHERE id = 2;
CREATE UNIQUE INDEX index_validation_key ON index_validation(label);
ROLLBACK TO SAVEPOINT before_validation;
INSERT INTO index_validation VALUES (3, 'ONE', true);
CREATE UNIQUE INDEX index_validation_key ON index_validation(lower(label)) WHERE active;
ROLLBACK TO SAVEPOINT before_validation;
INSERT INTO index_validation VALUES (3, NULL, true), (4, NULL, true);
CREATE UNIQUE INDEX index_validation_key ON index_validation(label) NULLS NOT DISTINCT;
ROLLBACK TO SAVEPOINT before_validation;
INSERT INTO index_validation VALUES (3, NULL, true), (4, NULL, true);
CREATE UNIQUE INDEX index_validation_key ON index_validation(label);
ROLLBACK TO SAVEPOINT before_validation;
DELETE FROM index_validation WHERE id = 1;
UPDATE index_validation SET label = 'one', active = true WHERE id = 2;
INSERT INTO index_validation VALUES (3, 'three', true);
CREATE UNIQUE INDEX index_validation_key ON index_validation(lower(label)) WHERE active;
COMMIT;
INSERT INTO index_validation VALUES (4, 'ONE', true);
SELECT id, label FROM index_validation ORDER BY id;
DROP TABLE index_validation;
