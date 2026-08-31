-- Durable table metadata: heap access method and per-column statistics are
-- visible through PostgreSQL catalogs without changing row contents.

CREATE TABLE table_metadata_rows (id integer, payload text) USING heap;
INSERT INTO table_metadata_rows VALUES (1, 'before');
ALTER TABLE table_metadata_rows SET ACCESS METHOD heap;
ALTER TABLE table_metadata_rows ALTER COLUMN payload SET STATISTICS 91;

SELECT relam FROM pg_class WHERE oid = 'table_metadata_rows'::regclass;
SELECT attname, attstattarget
  FROM pg_attribute
 WHERE attrelid = 'table_metadata_rows'::regclass AND attnum > 0
 ORDER BY attnum;
SELECT id, payload FROM table_metadata_rows;

DROP TABLE table_metadata_rows;
