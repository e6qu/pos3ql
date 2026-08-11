-- REINDEX rebuilds the bounded access structures from the authoritative table
-- image. The result rows prove that index, table, and schema target selection
-- preserves PostgreSQL-visible query semantics.
DROP TABLE IF EXISTS reindex_rows;
DROP SCHEMA IF EXISTS reindex_schema CASCADE;

CREATE TABLE reindex_rows (id int, value text);
INSERT INTO reindex_rows VALUES (1, 'one'), (2, 'two');
CREATE INDEX reindex_value ON reindex_rows (value);
REINDEX INDEX reindex_value;
SELECT id FROM reindex_rows WHERE value = 'two';
REINDEX TABLE reindex_rows;
SELECT id FROM reindex_rows WHERE value = 'one';

CREATE SCHEMA reindex_schema;
CREATE TABLE reindex_schema.rows (id int);
INSERT INTO reindex_schema.rows VALUES (7);
CREATE INDEX reindex_schema_rows ON reindex_schema.rows (id);
REINDEX SCHEMA reindex_schema;
SELECT id FROM reindex_schema.rows WHERE id = 7;

DROP TABLE reindex_rows;
DROP SCHEMA reindex_schema CASCADE;
