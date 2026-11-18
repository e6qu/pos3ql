-- A domain has a collation: the one COLLATE gives it, else its parent's or
-- its base type's. Columns, attributes and ALTER targets of the domain take
-- it unless they write their own COLLATE; a cast to a domain with a
-- non-default collation derives it. A custom collation is a dependency.
CREATE COLLATION c864 (provider = libc, locale = 'C');
CREATE DOMAIN dc864 AS text COLLATE "C";
CREATE DOMAIN dd864 AS text;
CREATE DOMAIN dn864 AS integer COLLATE "C";
CREATE DOMAIN dx864 AS text COLLATE "nosuch864";
CREATE DOMAIN dm864 AS text COLLATE "C" COLLATE "POSIX";
CREATE DOMAIN dp864 AS dc864;
CREATE DOMAIN dq864 AS dc864 COLLATE "POSIX";
CREATE DOMAIN dcu864 AS text COLLATE c864;
CREATE DOMAIN dk864 AS text DEFAULT 'x' COLLATE "C" CHECK (VALUE <> '');
CREATE DOMAIN da864 AS text[] COLLATE "C";
CREATE DOMAIN dnd864 AS integer;
CREATE DOMAIN dni864 AS dnd864 COLLATE "C";
SELECT typname, typcollation::regcollation FROM pg_type
  WHERE typname IN ('dc864','dd864','dp864','dq864','dcu864','da864','_dc864','_da864','dk864','dnd864')
  ORDER BY 1;
DROP DOMAIN dk864, da864, dnd864;
CREATE TABLE t864 (a dc864, b dd864, c dc864 COLLATE "POSIX", d dp864, e dq864, f dcu864,
  g dc864[], h dd864 COLLATE "C");
SELECT attname, attcollation::regcollation FROM pg_attribute
  WHERE attrelid = 't864'::regclass AND attnum > 0 ORDER BY attnum;
INSERT INTO t864 (a, b, c) VALUES ('x', 'y', 'z');
SELECT pg_collation_for('x'::dc864) AS k1, pg_collation_for('x'::dd864) AS k2,
  pg_collation_for('x'::text::dc864) AS k3, pg_collation_for(a) AS k4,
  pg_collation_for(b) AS k5, pg_collation_for(a::text) AS k6,
  pg_collation_for(b::dc864) AS k7, pg_collation_for(c::dc864) AS k8,
  pg_collation_for(a || 'y') AS k9, pg_collation_for(a::dp864) AS k10,
  pg_collation_for(a::dd864) AS k11, pg_collation_for(NULL::dc864) AS k12,
  pg_collation_for(('x'::text COLLATE "POSIX")::dc864) AS k13
  FROM t864;
SELECT pg_collation_for('x'::dc864 COLLATE "POSIX");
SELECT a < 'B' AS lt_c, b < 'B' AS lt_d FROM t864;
SELECT a || b FROM t864;
CREATE TABLE t2864 AS SELECT 'x'::dc864 AS a, 'y'::dcu864 AS b, a::dd864 AS c FROM t864;
SELECT attname, attcollation::regcollation FROM pg_attribute
  WHERE attrelid = 't2864'::regclass AND attnum > 0 ORDER BY attnum;
CREATE VIEW v864 AS SELECT a, a::dd864 AS aa, b::dc864 AS bb FROM t864;
SELECT attname, attcollation::regcollation FROM pg_attribute
  WHERE attrelid = 'v864'::regclass AND attnum > 0 ORDER BY attnum;
CREATE TYPE r864 AS (x dc864, y dd864, z dc864 COLLATE "POSIX");
ALTER TYPE r864 ADD ATTRIBUTE w dc864;
ALTER TYPE r864 ADD ATTRIBUTE w2 dc864 COLLATE "default";
SELECT attname, attcollation::regcollation FROM pg_attribute
  WHERE attrelid = 'r864'::regclass AND attnum > 0 ORDER BY attnum;
ALTER TABLE t864 ADD COLUMN i dc864;
ALTER TABLE t864 ADD COLUMN j dc864 COLLATE "default";
ALTER TABLE t864 ALTER COLUMN b TYPE text;
ALTER TABLE t864 ALTER COLUMN h TYPE text;
ALTER TABLE t864 ALTER COLUMN c TYPE text COLLATE "C";
SELECT attname, attcollation::regcollation FROM pg_attribute
  WHERE attrelid = 't864'::regclass AND attnum > 0 ORDER BY attnum;
CREATE INDEX ON t864 (a);
CREATE INDEX t864_b ON t864 (b);
SELECT indexrelid::regclass, indcollation::oid[] FROM pg_index
  WHERE indrelid = 't864'::regclass ORDER BY 1;
SELECT domain_name, collation_schema, collation_name FROM information_schema.domains
  WHERE domain_name LIKE 'd%864' ORDER BY 1;
SELECT deptype, classid::regclass, refclassid::regclass, refobjid::regcollation
  FROM pg_depend WHERE refclassid = 'pg_collation'::regclass
  AND refobjid = 'c864'::regcollation::oid ORDER BY 1, 2;
DROP COLLATION c864;
DROP COLLATION c864 CASCADE;
SELECT count(*) FROM pg_type WHERE typname = 'dcu864';
SELECT attname FROM pg_attribute
  WHERE attrelid = 't864'::regclass AND attnum > 0 ORDER BY attnum;
ALTER DOMAIN dc864 SET DEFAULT 'q';
SELECT typname, typcollation::regcollation FROM pg_type WHERE typname = 'dc864';
