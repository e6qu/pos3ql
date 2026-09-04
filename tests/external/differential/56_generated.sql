-- GENERATED ALWAYS AS (expr) STORED columns, matching PostgreSQL 18. The value
-- is computed from the row's other columns at insert/update; it cannot be
-- written explicitly (except to DEFAULT), and its expression must be immutable.
--
-- The differential corpora share one database; use distinctive names and drop
-- up front.
DROP TABLE IF EXISTS gen_g;
DROP TABLE IF EXISTS gen_h;

CREATE TABLE gen_g (
  a int,
  b int,
  c int GENERATED ALWAYS AS (a + b) STORED,
  label text GENERATED ALWAYS AS (a::text || '-' || b::text) STORED
);
INSERT INTO gen_g (a, b) VALUES (2, 3), (10, 20);
SELECT a, b, c, label FROM gen_g ORDER BY a;

-- A generated column cannot take an explicit non-DEFAULT value.
INSERT INTO gen_g (a, b, c) VALUES (1, 1, 5);
-- DEFAULT is allowed and computes the value.
INSERT INTO gen_g (a, b, c) VALUES (1, 1, DEFAULT);
SELECT a, b, c FROM gen_g WHERE a = 1;

-- Updating a dependency recomputes the generated column.
UPDATE gen_g SET b = 100 WHERE a = 2;
SELECT a, b, c FROM gen_g WHERE a = 2;
-- A generated column can only be updated to DEFAULT.
UPDATE gen_g SET c = 99 WHERE a = 10;
UPDATE gen_g SET c = DEFAULT WHERE a = 10;
SELECT a, c FROM gen_g WHERE a = 10;

-- attgenerated is 's' for a stored generated column.
SELECT attname, attgenerated FROM pg_attribute
  WHERE attrelid = 'gen_g'::regclass AND attnum > 0 ORDER BY attnum;

-- RETURNING sees the computed value.
INSERT INTO gen_g (a, b) VALUES (4, 5) RETURNING c, label;

-- Restrictions (all 42P17, except the subquery which is 0A000):
--   not immutable / references another generated column / self-reference /
--   subquery.
CREATE TABLE gen_bad1 (a int, x int GENERATED ALWAYS AS (now()) STORED);
CREATE TABLE gen_bad2 (a int, x int GENERATED ALWAYS AS (a) STORED,
                       y int GENERATED ALWAYS AS (x) STORED);
CREATE TABLE gen_bad3 (a int GENERATED ALWAYS AS (a) STORED);
CREATE TABLE gen_bad4 (a int, x int GENERATED ALWAYS AS ((SELECT 1)) STORED);

-- ADD COLUMN with a generated expression backfills existing rows.
CREATE TABLE gen_h (a int);
INSERT INTO gen_h VALUES (5), (7);
ALTER TABLE gen_h ADD COLUMN d int GENERATED ALWAYS AS (a * 10) STORED;
SELECT a, d FROM gen_h ORDER BY a;
INSERT INTO gen_h (a) VALUES (9);
SELECT a, d FROM gen_h ORDER BY a;

-- SET EXPRESSION rewrites existing values. DROP EXPRESSION is a distinct
-- transition: it preserves those values, clears attgenerated, and makes the
-- column writable. Defaults cannot silently replace a generated expression.
CREATE TABLE gen_evolution (a int, b int GENERATED ALWAYS AS (a + 1) STORED);
INSERT INTO gen_evolution (a) VALUES (2), (4);
ALTER TABLE gen_evolution ALTER COLUMN b SET EXPRESSION AS (a * 10);
SELECT a, b FROM gen_evolution ORDER BY a;
ALTER TABLE gen_evolution ALTER COLUMN b SET DEFAULT 7;
ALTER TABLE gen_evolution ALTER COLUMN b DROP DEFAULT;
ALTER TABLE gen_evolution ALTER COLUMN b SET NOT NULL;
ALTER TABLE gen_evolution ALTER COLUMN b ADD GENERATED ALWAYS AS IDENTITY;
ALTER TABLE gen_evolution ALTER COLUMN b DROP EXPRESSION;
SELECT a, b, attgenerated FROM gen_evolution
  CROSS JOIN pg_attribute
 WHERE attrelid = 'gen_evolution'::regclass AND attname = 'b'
 ORDER BY a;
UPDATE gen_evolution SET b = 99 WHERE a = 2;
INSERT INTO gen_evolution VALUES (7, 8);
SELECT a, b FROM gen_evolution ORDER BY a;
ALTER TABLE gen_evolution ALTER COLUMN b DROP EXPRESSION IF EXISTS;

-- LIKE copies a generated column as plain by default, keeps it with INCLUDING
-- GENERATED.
DROP TABLE IF EXISTS gen_cp1;
DROP TABLE IF EXISTS gen_cp2;
CREATE TABLE gen_cp1 (LIKE gen_g);
CREATE TABLE gen_cp2 (LIKE gen_g INCLUDING GENERATED);
SELECT attrelid::regclass::text AS tbl, attname, attgenerated FROM pg_attribute
  WHERE attrelid IN ('gen_cp1'::regclass, 'gen_cp2'::regclass) AND attnum > 0
  ORDER BY 1, attnum;

DROP TABLE gen_cp1;
DROP TABLE gen_cp2;
DROP TABLE gen_g;
DROP TABLE gen_h;
DROP TABLE gen_evolution;
