DROP TABLE IF EXISTS differential_sample_partition CASCADE;
DROP TABLE IF EXISTS differential_table_insert;
DROP TABLE IF EXISTS differential_table_copy;
DROP VIEW IF EXISTS differential_sample_view;
DROP VIEW IF EXISTS differential_table_view;
DROP TABLE IF EXISTS differential_table_source CASCADE;

CREATE TABLE differential_table_source (id integer PRIMARY KEY, label text);
INSERT INTO differential_table_source
SELECT value, 'value-' || value FROM generate_series(1,100) value;

TABLE differential_table_source ORDER BY id DESC LIMIT 2;
WITH copied AS (TABLE differential_table_source)
SELECT count(*), min(id), max(id) FROM copied;
SELECT renamed_id, renamed_label
FROM differential_table_source AS renamed(renamed_id, renamed_label)
ORDER BY renamed_id LIMIT 1;

SELECT count(*) FROM differential_table_source TABLESAMPLE SYSTEM (0);
SELECT count(*) FROM differential_table_source TABLESAMPLE SYSTEM (100);
SELECT count(*) FROM differential_table_source TABLESAMPLE BERNOULLI (0);
SELECT count(*) FROM differential_table_source TABLESAMPLE BERNOULLI (100);
SELECT count(*) FROM differential_table_source TABLESAMPLE SYSTEM ((SELECT 100));
SELECT
  (SELECT array_agg(id ORDER BY id)
     FROM differential_table_source TABLESAMPLE BERNOULLI (35) REPEATABLE (42))
  IS NOT DISTINCT FROM
  (SELECT array_agg(id ORDER BY id)
     FROM differential_table_source TABLESAMPLE BERNOULLI (35) REPEATABLE (42));

CREATE VIEW differential_table_view AS TABLE differential_table_source;
CREATE VIEW differential_sample_view AS
SELECT id, label FROM differential_table_source
TABLESAMPLE BERNOULLI (100) REPEATABLE (7);
CREATE TABLE differential_table_copy AS TABLE differential_table_view WITH DATA;
CREATE TABLE differential_table_insert (id integer PRIMARY KEY, label text);
INSERT INTO differential_table_insert TABLE differential_sample_view;
SELECT count(*) FROM differential_table_view;
SELECT count(*) FROM differential_sample_view;
SELECT count(*) FROM differential_table_copy;
SELECT count(*) FROM differential_table_insert;

CREATE TABLE differential_sample_partition (id integer) PARTITION BY RANGE (id);
CREATE TABLE differential_sample_partition_low
PARTITION OF differential_sample_partition FOR VALUES FROM (0) TO (10);
INSERT INTO differential_sample_partition VALUES (1), (2);
SELECT count(*) FROM differential_sample_partition;
SELECT count(*) FROM ONLY differential_sample_partition;
SELECT count(*) FROM (TABLE ONLY (differential_sample_partition)) AS parent_rows;

BEGIN;
DECLARE differential_table_cursor CURSOR FOR TABLE differential_table_view;
FETCH FORWARD 2 FROM differential_table_cursor;
COMMIT;

COPY (TABLE differential_sample_view) TO STDOUT;

SELECT * FROM differential_table_source TABLESAMPLE BERNOULLI (NULL);
SELECT * FROM differential_table_source TABLESAMPLE SYSTEM (101);
SELECT * FROM differential_table_source TABLESAMPLE BERNOULLI (10) REPEATABLE (NULL);
SELECT * FROM differential_table_source TABLESAMPLE missing (10);
SELECT * FROM differential_table_view TABLESAMPLE SYSTEM (10);
SELECT * FROM differential_table_source TABLESAMPLE SYSTEM (id);
SELECT * FROM differential_table_source AS sampled TABLESAMPLE SYSTEM (sampled.id);
SELECT * FROM differential_table_source TABLESAMPLE SYSTEM (count(*));
SELECT * FROM differential_table_source TABLESAMPLE SYSTEM (row_number() OVER ());
SELECT * FROM differential_table_source TABLESAMPLE SYSTEM (generate_series(1, 2));

DROP TABLE differential_sample_partition CASCADE;
DROP TABLE differential_table_insert;
DROP TABLE differential_table_copy;
DROP VIEW differential_sample_view;
DROP VIEW differential_table_view;
DROP TABLE differential_table_source;
