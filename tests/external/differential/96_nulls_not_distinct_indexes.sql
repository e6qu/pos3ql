DROP TABLE IF EXISTS nulls_not_distinct_rows;
CREATE TABLE nulls_not_distinct_rows (key integer, payload text, active boolean);
CREATE UNIQUE INDEX nulls_not_distinct_key
  ON nulls_not_distinct_rows (key) NULLS NOT DISTINCT;
INSERT INTO nulls_not_distinct_rows VALUES (NULL, 'first', true);
INSERT INTO nulls_not_distinct_rows VALUES (NULL, 'duplicate', true);
INSERT INTO nulls_not_distinct_rows VALUES (NULL, 'ignored', true) ON CONFLICT (key) DO NOTHING;
INSERT INTO nulls_not_distinct_rows VALUES (1, 'one', true), (2, 'two', true);
CREATE UNIQUE INDEX nulls_not_distinct_active_payload
  ON nulls_not_distinct_rows (payload) NULLS NOT DISTINCT WHERE active;
INSERT INTO nulls_not_distinct_rows VALUES (3, NULL, true);
INSERT INTO nulls_not_distinct_rows VALUES (4, NULL, true);
SELECT key, payload FROM nulls_not_distinct_rows ORDER BY key NULLS FIRST;
SELECT indexdef FROM pg_indexes
  WHERE tablename = 'nulls_not_distinct_rows' ORDER BY indexname;
SELECT indnullsnotdistinct FROM pg_index index_catalog
  JOIN pg_class relation ON relation.oid = index_catalog.indexrelid
  WHERE relation.relname = 'nulls_not_distinct_key';
CREATE INDEX nulls_not_distinct_non_unique
  ON nulls_not_distinct_rows (payload) NULLS NOT DISTINCT;
SELECT indexdef FROM pg_indexes
  WHERE indexname = 'nulls_not_distinct_non_unique';
SELECT indnullsnotdistinct FROM pg_index index_catalog
  JOIN pg_class relation ON relation.oid = index_catalog.indexrelid
  WHERE relation.relname = 'nulls_not_distinct_non_unique';
DROP TABLE nulls_not_distinct_rows;
