-- Index definitions remain executable catalog state through partition
-- recursion, attachment, mutation, rebuild, and drop.
DROP TABLE IF EXISTS index_lifecycle_root CASCADE;
DROP TABLE IF EXISTS index_lifecycle_concurrent;

CREATE TABLE index_lifecycle_root (id integer, value text, payload text)
PARTITION BY RANGE (id);
CREATE TABLE index_lifecycle_low PARTITION OF index_lifecycle_root
FOR VALUES FROM (0) TO (10);
CREATE TABLE index_lifecycle_high PARTITION OF index_lifecycle_root
FOR VALUES FROM (10) TO (20);
INSERT INTO index_lifecycle_root VALUES (1, 'one', 'low'), (11, 'eleven', 'high');

CREATE INDEX index_lifecycle_value ON index_lifecycle_root
(id, value COLLATE "C" text_ops DESC NULLS LAST) INCLUDE (payload)
WITH (fillfactor = 80, deduplicate_items = off) WHERE id >= 0;
SELECT indexed_table.relname, relation.relkind, relation.relispartition,
       index.indisvalid, relation.reloptions
FROM pg_class relation
JOIN pg_index index ON index.indexrelid = relation.oid
JOIN pg_class indexed_table ON indexed_table.oid = index.indrelid
WHERE indexed_table.relname IN
      ('index_lifecycle_root', 'index_lifecycle_low', 'index_lifecycle_high')
ORDER BY indexed_table.relname;
SELECT count(*)
FROM pg_inherits inheritance
JOIN pg_class relation ON relation.oid = inheritance.inhrelid
JOIN pg_index index ON index.indexrelid = relation.oid
JOIN pg_class indexed_table ON indexed_table.oid = index.indrelid
WHERE relation.relkind IN ('i', 'I')
  AND indexed_table.relname IN
      ('index_lifecycle_root', 'index_lifecycle_low', 'index_lifecycle_high');

CREATE INDEX index_lifecycle_only ON ONLY index_lifecycle_root (id);
CREATE INDEX index_lifecycle_low_attached ON index_lifecycle_low (id);
CREATE INDEX index_lifecycle_high_attached ON index_lifecycle_high (id);
SELECT indisvalid FROM pg_index
WHERE indexrelid = 'index_lifecycle_only'::regclass;
ALTER INDEX index_lifecycle_only ATTACH PARTITION index_lifecycle_low_attached;
SELECT indisvalid FROM pg_index
WHERE indexrelid = 'index_lifecycle_only'::regclass;
ALTER INDEX index_lifecycle_only ATTACH PARTITION index_lifecycle_high_attached;
SELECT indisvalid FROM pg_index
WHERE indexrelid = 'index_lifecycle_only'::regclass;

CREATE INDEX index_lifecycle_expression ON index_lifecycle_low ((lower(value)));
ALTER INDEX index_lifecycle_expression ALTER COLUMN 1 SET STATISTICS 77;
ALTER INDEX index_lifecycle_expression SET (fillfactor = 70, deduplicate_items = on);
SELECT attribute.attname, attribute.atttypid, attribute.attstattarget,
       relation.reloptions
FROM pg_attribute attribute
JOIN pg_class relation ON relation.oid = attribute.attrelid
WHERE relation.relname = 'index_lifecycle_expression' AND attribute.attnum = 1;
REINDEX INDEX index_lifecycle_expression;

CREATE TABLE index_lifecycle_concurrent (id integer, value text);
CREATE INDEX CONCURRENTLY index_lifecycle_concurrent_value
ON index_lifecycle_concurrent (value);
REINDEX (CONCURRENTLY) INDEX index_lifecycle_concurrent_value;
DROP INDEX CONCURRENTLY index_lifecycle_concurrent_value;

DROP INDEX index_lifecycle_value;
SELECT count(*) FROM pg_indexes
WHERE indexname = 'index_lifecycle_value'
   OR indexname LIKE 'index_lifecycle_%_id_idx';
DROP TABLE index_lifecycle_root CASCADE;
DROP TABLE index_lifecycle_concurrent;
