-- Non-constant column DEFAULTs, matching PostgreSQL 18. A default with a
-- function call (now(), nextval(...), …) is evaluated per inserted row, not
-- folded once at CREATE TABLE time.
--
-- The differential corpora share one database, so drop this file's objects up
-- front (distinctive names, but be defensive) to avoid collisions.
DROP TABLE IF EXISTS de_d;
DROP TABLE IF EXISTS de_t;
DROP TABLE IF EXISTS de_u;
DROP SEQUENCE IF EXISTS de_seq;

CREATE SEQUENCE de_seq;
CREATE TABLE de_d (
  id   bigint DEFAULT nextval('de_seq'),
  n    int    DEFAULT 2 + 3,
  note text   DEFAULT 'x'
);

-- A DEFAULT nextval advances once per inserted row; the constant defaults fold.
INSERT INTO de_d (note) VALUES ('a'), ('b');
INSERT INTO de_d DEFAULT VALUES;
-- An explicitly supplied value does NOT advance the sequence.
INSERT INTO de_d (id, note) VALUES (100, 'explicit');
INSERT INTO de_d (note) VALUES ('c');
SELECT id, n, note FROM de_d ORDER BY id;

-- DEFAULT VALUES over multiple statements keeps advancing the sequence.
INSERT INTO de_d DEFAULT VALUES;
SELECT max(id) FROM de_d WHERE note = 'x';

-- atthasdef is true for a column with any default (constant or expression).
SELECT attname, atthasdef FROM pg_attribute
  WHERE attrelid = 'de_d'::regclass AND attnum > 0 ORDER BY attnum;

-- now() is evaluated per INSERT statement (all rows of one statement share it),
-- not frozen at CREATE TABLE time — so a row inserted later differs from an
-- earlier one, while two rows of the same statement match.
CREATE TABLE de_t (ts timestamptz DEFAULT now(), tag text);
INSERT INTO de_t (tag) VALUES ('x'), ('y');
SELECT count(DISTINCT ts) AS distinct_ts, count(*) AS rows FROM de_t;

-- ALTER COLUMN SET DEFAULT with an expression; a later insert picks it up.
CREATE TABLE de_u (a int, b bigint);
ALTER TABLE de_u ALTER COLUMN b SET DEFAULT nextval('de_seq');
INSERT INTO de_u (a) VALUES (1);
INSERT INTO de_u (a, b) VALUES (2, 999);
INSERT INTO de_u (a) VALUES (3);
SELECT a, b FROM de_u ORDER BY a;

-- ADD COLUMN with a non-constant DEFAULT; DROP DEFAULT removes it.
ALTER TABLE de_u ADD COLUMN c bigint DEFAULT nextval('de_seq');
INSERT INTO de_u (a) VALUES (4);
SELECT a, c FROM de_u WHERE a = 4;
ALTER TABLE de_u ALTER COLUMN c DROP DEFAULT;
INSERT INTO de_u (a) VALUES (5);
SELECT a, c FROM de_u WHERE a = 5;

-- A DEFAULT referencing a sequence is dropped independently of the sequence.
DROP TABLE de_d;
DROP TABLE de_t;
DROP TABLE de_u;
DROP SEQUENCE de_seq;
