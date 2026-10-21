-- Typed tables keep the composite identity, not the spelling, at every
-- PostgreSQL-visible catalog and dependency boundary.
DROP TABLE IF EXISTS differential_typed_rows CASCADE;
DROP TYPE IF EXISTS differential_typed_row CASCADE;

CREATE TYPE differential_typed_row AS (id integer, label text);
CREATE TABLE differential_typed_rows OF differential_typed_row;
INSERT INTO differential_typed_rows VALUES (1, 'one'), (2, 'two');

SELECT id, label FROM differential_typed_rows ORDER BY id;
SELECT c.reloftype = t.oid
FROM pg_class AS c
JOIN pg_type AS t ON t.typname = 'differential_typed_row'
WHERE c.relname = 'differential_typed_rows';

ALTER TYPE differential_typed_row RENAME TO differential_typed_row_moved;
SELECT c.reloftype = t.oid
FROM pg_class AS c
JOIN pg_type AS t ON t.typname = 'differential_typed_row_moved'
WHERE c.relname = 'differential_typed_rows';
SELECT id, label FROM differential_typed_rows ORDER BY id;

SET client_min_messages = warning;
DROP TYPE differential_typed_row_moved CASCADE;
RESET client_min_messages;
DROP TYPE IF EXISTS differential_typed_row_moved;

CREATE TABLE differential_table_storage (id integer) WITH (fillfactor = 80);
ALTER TABLE differential_table_storage SET (fillfactor = 70);
SELECT reloptions::text FROM pg_class WHERE relname = 'differential_table_storage';
ALTER TABLE differential_table_storage RESET (fillfactor);
SELECT reloptions IS NULL FROM pg_class WHERE relname = 'differential_table_storage';
DROP TABLE differential_table_storage;

CREATE TABLE differential_detach_parent (id integer) PARTITION BY RANGE (id);
CREATE TABLE differential_detach_child PARTITION OF differential_detach_parent
  FOR VALUES FROM (0) TO (10);
INSERT INTO differential_detach_parent VALUES (4);
ALTER TABLE differential_detach_parent
  DETACH PARTITION differential_detach_child CONCURRENTLY;
SELECT count(*) FROM differential_detach_parent;
SELECT count(*) FROM differential_detach_child;
SELECT relispartition FROM pg_class WHERE relname = 'differential_detach_child';
DROP TABLE differential_detach_child, differential_detach_parent;

CREATE SCHEMA typed_qualified;
CREATE TYPE public.typed_shadow AS (wrong text);
CREATE TYPE typed_qualified.typed_shadow AS (id integer);
CREATE TABLE qualified_typed_rows OF typed_qualified.typed_shadow;
INSERT INTO qualified_typed_rows VALUES (7);
SELECT id FROM qualified_typed_rows;
CREATE TYPE public."typed.dot" AS (id integer);
CREATE TABLE quoted_typed_rows OF "typed.dot";
CREATE TABLE altered_quoted_rows (id integer);
ALTER TABLE altered_quoted_rows OF "typed.dot";
INSERT INTO quoted_typed_rows VALUES (8);
INSERT INTO altered_quoted_rows VALUES (9);
SELECT id FROM quoted_typed_rows UNION ALL SELECT id FROM altered_quoted_rows ORDER BY id;
CREATE TYPE public.wrong_typed_source AS ENUM ('x');
CREATE TABLE rejected_enum_rows OF wrong_typed_source;
CREATE TABLE rejected_builtin_rows OF pg_catalog.int4;
CREATE TABLE rejected_relation_rows OF qualified_typed_rows;
CREATE TYPE typed_qualified.wrong_typed_source AS (id integer);
SET search_path = public, typed_qualified;
CREATE TABLE rejected_shadowed_rows OF wrong_typed_source;
RESET search_path;
DROP TABLE qualified_typed_rows, quoted_typed_rows, altered_quoted_rows;
DROP TYPE public.typed_shadow, public."typed.dot", public.wrong_typed_source;
DROP SCHEMA typed_qualified CASCADE;
