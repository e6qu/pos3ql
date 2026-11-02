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

CREATE TABLE reindex_deferred (
  id integer UNIQUE DEFERRABLE INITIALLY DEFERRED,
  payload integer
);
CREATE INDEX reindex_payload ON reindex_deferred(payload);
CREATE UNIQUE INDEX reindex_payload_unique ON reindex_deferred(payload);
BEGIN;
INSERT INTO reindex_deferred VALUES(1,10),(1,20);
REINDEX INDEX reindex_payload;
SELECT payload FROM reindex_deferred ORDER BY payload;
ROLLBACK;
BEGIN;
INSERT INTO reindex_deferred VALUES(1,10),(1,20);
REINDEX INDEX reindex_payload_unique;
SELECT payload FROM reindex_deferred ORDER BY payload;
ROLLBACK;
BEGIN;
INSERT INTO reindex_deferred VALUES(1,10),(1,20);
REINDEX TABLE reindex_deferred;
ROLLBACK;
BEGIN;
INSERT INTO reindex_deferred VALUES(1,10),(1,20);
REINDEX INDEX reindex_deferred_id_key;
ROLLBACK;
SELECT count(*) FROM reindex_deferred;
DROP TABLE reindex_deferred;

CREATE TABLE reindex_excluded (
  r int4range,
  CONSTRAINT reindex_excluded_key
    EXCLUDE USING gist(r WITH &&) DEFERRABLE INITIALLY DEFERRED
);
BEGIN;
INSERT INTO reindex_excluded VALUES('[1,4)'),('[3,6)');
REINDEX TABLE reindex_excluded;
ROLLBACK;
BEGIN;
INSERT INTO reindex_excluded VALUES('[1,4)'),('[3,6)');
REINDEX INDEX reindex_excluded_key;
ROLLBACK;
SELECT count(*) FROM reindex_excluded;
DROP TABLE reindex_excluded;
