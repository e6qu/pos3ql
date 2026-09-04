DROP TABLE IF EXISTS table_column_metadata;

CREATE TABLE table_column_metadata (
  id integer,
  body text STORAGE MAIN COMPRESSION pglz,
  payload bytea STORAGE EXTERNAL COMPRESSION pglz
);

SELECT attname, attstorage, attcompression
  FROM pg_attribute
 WHERE attrelid = 'table_column_metadata'::regclass
   AND attnum > 0
 ORDER BY attnum;

ALTER TABLE table_column_metadata ALTER COLUMN body SET STORAGE EXTENDED;
ALTER TABLE table_column_metadata ALTER COLUMN body SET COMPRESSION DEFAULT;
ALTER TABLE table_column_metadata ALTER COLUMN payload SET COMPRESSION pglz;
ALTER TABLE table_column_metadata ALTER COLUMN body
  SET (n_distinct = 11, n_distinct_inherited = -0.25);

SELECT attname, attstorage, attcompression
  FROM pg_attribute
 WHERE attrelid = 'table_column_metadata'::regclass
   AND attnum > 0
 ORDER BY attnum;

SELECT attoptions::text FROM pg_attribute
 WHERE attrelid = 'table_column_metadata'::regclass AND attname = 'body';

INSERT INTO table_column_metadata (id, body)
  VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd');
ANALYZE table_column_metadata;
SELECT n_distinct FROM pg_stats
 WHERE tablename = 'table_column_metadata' AND attname = 'body';

ALTER TABLE table_column_metadata ALTER COLUMN body RESET (n_distinct);
SELECT attoptions::text FROM pg_attribute
 WHERE attrelid = 'table_column_metadata'::regclass AND attname = 'body';

ALTER TABLE table_column_metadata ALTER COLUMN payload SET STORAGE DEFAULT;
SELECT attstorage FROM pg_attribute
 WHERE attrelid = 'table_column_metadata'::regclass AND attname = 'payload';

CREATE TABLE table_column_metadata_like (LIKE table_column_metadata INCLUDING STORAGE INCLUDING COMPRESSION);
CREATE TABLE table_column_metadata_like_default (LIKE table_column_metadata);

SELECT c.relname, a.attname, a.attstorage, a.attcompression
  FROM pg_attribute a
  JOIN pg_class c ON c.oid = a.attrelid
 WHERE c.relname IN ('table_column_metadata_like', 'table_column_metadata_like_default')
   AND a.attnum > 0
 ORDER BY c.relname, a.attnum;

DROP TABLE table_column_metadata_like;
DROP TABLE table_column_metadata_like_default;
DROP TABLE table_column_metadata;
