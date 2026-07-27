-- CREATE DOMAIN, matching PostgreSQL 18. A domain is a base type plus optional
-- NOT NULL / DEFAULT / CHECK(VALUE) constraints, enforced when a value is
-- coerced into a column of the domain. pg_typeof reports the domain on a bare
-- column and the base type through any expression.
--
-- Distinctive names + drop up front (the differential corpora share a database).
DROP TABLE IF EXISTS dom_t;
DROP TABLE IF EXISTS dom_t2;
DROP TABLE IF EXISTS dom_nested;
DROP TABLE IF EXISTS dom_invalid;
DROP DOMAIN IF EXISTS dom_small;
DROP DOMAIN IF EXISTS dom_pos;
DROP DOMAIN IF EXISTS dom_email;
DROP DOMAIN IF EXISTS dom_def;

CREATE DOMAIN dom_pos AS int CONSTRAINT gt0 CHECK (VALUE > 0) CONSTRAINT lt100 CHECK (VALUE < 100);
CREATE DOMAIN dom_email AS text NOT NULL CHECK (VALUE LIKE '%@%');
CREATE DOMAIN dom_def AS int DEFAULT 42 CHECK (VALUE >= 0);

CREATE TABLE dom_t (id dom_pos, e dom_email, d dom_def);
INSERT INTO dom_t VALUES (5, 'a@b.com', 7);
-- Column omitted → domain default applies.
INSERT INTO dom_t (id, e) VALUES (9, 'x@y.com');
SELECT id, e, d FROM dom_t ORDER BY id;

-- pg_typeof: the domain on a bare column, the base type through an expression.
SELECT pg_typeof(id), pg_typeof(e), pg_typeof(d) FROM dom_t WHERE id = 5;
SELECT pg_typeof(id + 1), pg_typeof(d * 2) FROM dom_t WHERE id = 5;

-- Explicit casts use the same base coercion + complete constraint chain.
SELECT 5::dom_pos, pg_typeof(5::dom_pos);
SELECT 0::dom_pos;

-- Constraint violations: CHECK (23514, naming the constraint) and NOT NULL (23502).
INSERT INTO dom_t VALUES (-1, 'a@b.com', 0);
INSERT INTO dom_t VALUES (200, 'a@b.com', 0);
INSERT INTO dom_t VALUES (5, 'bad', 0);
INSERT INTO dom_t VALUES (5, NULL, 0);
INSERT INTO dom_t VALUES (5, 'a@b.com', -3);

-- pg_type reflects the domain: typtype 'd', base type, NOT NULL, default.
SELECT typname, typtype, typbasetype, typnotnull, typdefault
  FROM pg_type WHERE typname IN ('dom_pos', 'dom_email', 'dom_def') ORDER BY typname;

-- ALTER DOMAIN: add / drop a CHECK, set / drop a default, set / drop NOT NULL.
ALTER DOMAIN dom_def SET DEFAULT 99;
CREATE TABLE dom_t2 (x dom_def);
INSERT INTO dom_t2 DEFAULT VALUES;
SELECT x FROM dom_t2;
ALTER DOMAIN dom_def DROP DEFAULT;
-- ADD a CHECK every existing row already satisfies (so no re-validation
-- failure), then enforce it on a new insert.
ALTER DOMAIN dom_pos ADD CONSTRAINT lt50 CHECK (VALUE < 50);
INSERT INTO dom_t (id, e) VALUES (60, 'q@w.com'); -- 60 >= 50 → violates lt50
INSERT INTO dom_t (id, e) VALUES (8, 'q@w.com');  -- passes
ALTER DOMAIN dom_pos DROP CONSTRAINT lt50;
INSERT INTO dom_t (id, e) VALUES (60, 'q@w.com'); -- allowed again after DROP
SELECT count(*) FROM dom_t;

-- ALTER DOMAIN SET / DROP NOT NULL.
ALTER DOMAIN dom_pos SET NOT NULL;
ALTER DOMAIN dom_pos DROP NOT NULL;

-- DROP DOMAIN with a dependent column is 2BP01 (RESTRICT default).
DROP DOMAIN dom_pos;

-- A domain over a domain keeps its immediate catalog parent, inherited
-- constraints, and the parent default copied at CREATE time. Its generated
-- array type validates every element.
CREATE DOMAIN dom_small AS dom_pos DEFAULT 7 CHECK (VALUE < 10);
CREATE TABLE dom_nested (x dom_small, xs dom_small[]);
INSERT INTO dom_nested VALUES (DEFAULT, ARRAY[1,2,9]::dom_small[]);
SELECT x, xs, pg_typeof(x), pg_typeof(xs) FROM dom_nested;
SELECT ARRAY[1,0]::dom_small[];
SELECT ARRAY[1,10]::dom_small[];
SELECT t.typname, t.typtype, b.typname AS base_type,
       e.typname AS element_type, a.typname AS array_type
  FROM pg_type t
  LEFT JOIN pg_type b ON b.oid = t.typbasetype
  LEFT JOIN pg_type e ON e.oid = t.typelem
  LEFT JOIN pg_type a ON a.oid = t.typarray
 WHERE t.typname IN ('dom_pos', 'dom_small', '_dom_pos', '_dom_small')
 ORDER BY t.typname;

-- Strengthening a domain validates stored rows and leaves the old definition
-- installed if validation fails.
CREATE DOMAIN dom_invalid AS int;
CREATE TABLE dom_invalid_t (x dom_invalid);
INSERT INTO dom_invalid_t VALUES (8);
ALTER DOMAIN dom_invalid ADD CHECK (VALUE < 5);
INSERT INTO dom_invalid_t VALUES (9);
SELECT x FROM dom_invalid_t ORDER BY x;

-- ALTER DOMAIN DROP CONSTRAINT on a missing constraint is 42704.
ALTER DOMAIN dom_pos DROP CONSTRAINT nope;

-- An unknown type name is 42704.
CREATE TABLE dom_bad (a nonesuch);

DROP TABLE dom_t;
DROP TABLE dom_t2;
DROP TABLE dom_nested;
DROP TABLE dom_invalid_t;
DROP DOMAIN dom_small;
DROP DOMAIN dom_pos;
DROP DOMAIN dom_email;
DROP DOMAIN dom_def;
DROP DOMAIN dom_invalid;
