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

DROP TYPE differential_typed_row_moved CASCADE;
DROP TYPE IF EXISTS differential_typed_row_moved;
