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
