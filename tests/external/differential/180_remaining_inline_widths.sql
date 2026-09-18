-- Former maintenance and set-returning-function inline widths must not become
-- client-visible limits. This file runs unchanged on PostgreSQL 18.

CREATE TABLE inline_width_split(value text);
INSERT INTO inline_width_split
SELECT value
FROM string_to_table(repeat('x,', 1099) || 'x', ',') AS split(value);
SELECT count(*), min(length(value)), max(length(value))
FROM inline_width_split;

SELECT count(*), min(length(value)), max(length(value))
FROM regexp_split_to_table(repeat('y,', 1099) || 'y', ',') AS split(value);

CREATE SCHEMA a;
CREATE SCHEMA b;
CREATE SCHEMA c;
CREATE SCHEMA d;
CREATE SCHEMA e;
CREATE SCHEMA f;
CREATE SCHEMA g;
CREATE SCHEMA h;
CREATE SCHEMA i;
CREATE SCHEMA j;
CREATE SCHEMA k;
CREATE SCHEMA l;
CREATE SCHEMA m;
CREATE SCHEMA n;
CREATE SCHEMA o;
CREATE SCHEMA p;
CREATE SCHEMA q;
CREATE SCHEMA r;
CREATE SCHEMA s;
CREATE SCHEMA t;
CREATE TABLE t.path_target(value integer);
INSERT INTO t.path_target VALUES (19);
SET search_path TO a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r,
  s, t, public;
SELECT value FROM path_target;
SELECT current_schema(), current_schemas(true);
RESET search_path;

-- simulator-backed external runs
SELECT count(*)
FROM (
  SELECT string_to_table(source, ',') AS value
  FROM (SELECT repeat('x,', 1099) || 'x' AS source) AS input
) AS split;
SELECT count(*)
FROM (
  SELECT regexp_split_to_table(source, ',') AS value
  FROM (SELECT repeat('y,', 1099) || 'y' AS source) AS input
) AS split;

-- cleanup
DROP TABLE inline_width_split;
DROP SCHEMA a;
DROP SCHEMA b;
DROP SCHEMA c;
DROP SCHEMA d;
DROP SCHEMA e;
DROP SCHEMA f;
DROP SCHEMA g;
DROP SCHEMA h;
DROP SCHEMA i;
DROP SCHEMA j;
DROP SCHEMA k;
DROP SCHEMA l;
DROP SCHEMA m;
DROP SCHEMA n;
DROP SCHEMA o;
DROP SCHEMA p;
DROP SCHEMA q;
DROP SCHEMA r;
DROP SCHEMA s;
DROP SCHEMA t CASCADE;
