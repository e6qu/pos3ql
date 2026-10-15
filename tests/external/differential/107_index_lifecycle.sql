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

-- Recursive index creation validates earlier changes in every leaf. A later
-- child's failure rolls back the root and already completed child indexes.
CREATE TABLE partition_validation(bucket integer, label text, active boolean, divisor integer)
PARTITION BY RANGE (bucket);
CREATE TABLE partition_validation_low PARTITION OF partition_validation
FOR VALUES FROM (0) TO (10);
CREATE TABLE partition_validation_high PARTITION OF partition_validation
FOR VALUES FROM (10) TO (20);
INSERT INTO partition_validation VALUES
(1, 'one', true, 10), (1, 'two', false, 20),
(11, 'eleven', true, 10), (11, 'twelve', true, 20);
BEGIN;
SAVEPOINT before_partition_index;
INSERT INTO partition_validation VALUES (11, 'fresh', true, 30), (11, 'fresh', true, 40);
CREATE UNIQUE INDEX partition_validated ON partition_validation(bucket, label);
ROLLBACK TO SAVEPOINT before_partition_index;
SELECT count(*) FROM pg_indexes WHERE tablename IN
('partition_validation', 'partition_validation_low', 'partition_validation_high');
UPDATE partition_validation SET label = 'eleven' WHERE label = 'twelve';
CREATE UNIQUE INDEX partition_validated ON partition_validation(bucket, label);
ROLLBACK TO SAVEPOINT before_partition_index;
INSERT INTO partition_validation VALUES (1, 'ONE', true, 30);
CREATE UNIQUE INDEX partition_validated ON partition_validation(bucket, lower(label)) WHERE active;
ROLLBACK TO SAVEPOINT before_partition_index;
INSERT INTO partition_validation VALUES (11, NULL, true, 30), (11, NULL, true, 40);
CREATE UNIQUE INDEX partition_validated ON partition_validation(bucket, label) NULLS NOT DISTINCT;
ROLLBACK TO SAVEPOINT before_partition_index;
UPDATE partition_validation SET divisor = 0 WHERE label = 'twelve';
CREATE INDEX partition_validated ON partition_validation(bucket, (100 / divisor));
ROLLBACK TO SAVEPOINT before_partition_index;
SELECT count(*) FROM pg_indexes WHERE tablename IN
('partition_validation', 'partition_validation_low', 'partition_validation_high');
DELETE FROM partition_validation WHERE label = 'one';
UPDATE partition_validation SET label = 'one', active = true WHERE label = 'two';
INSERT INTO partition_validation VALUES (11, 'thirteen', true, 30);
CREATE UNIQUE INDEX partition_validated ON partition_validation(bucket, lower(label)) WHERE active;
COMMIT;
INSERT INTO partition_validation VALUES (11, 'ELEVEN', true, 40);
SELECT bucket, label FROM partition_validation ORDER BY bucket, label;
SELECT count(*) FROM pg_indexes WHERE tablename IN
('partition_validation', 'partition_validation_low', 'partition_validation_high');
DROP TABLE partition_validation CASCADE;
