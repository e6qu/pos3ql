DROP TABLE IF EXISTS like_metadata_target_excluded;
DROP TABLE IF EXISTS like_metadata_target;
DROP TABLE IF EXISTS like_metadata_source;

CREATE TABLE like_metadata_source (
  id integer NOT NULL,
  value integer,
  label text,
  CONSTRAINT like_metadata_source_check CHECK (value > 0)
);
CREATE INDEX like_metadata_source_label_idx ON like_metadata_source(label);
CREATE STATISTICS like_metadata_source_value_label_stats (ndistinct, dependencies)
  ON value, label FROM like_metadata_source;

COMMENT ON TABLE like_metadata_source IS 'source relation is not copied';
COMMENT ON COLUMN like_metadata_source.label IS 'copied column';
COMMENT ON CONSTRAINT like_metadata_source_check ON like_metadata_source
  IS 'copied check';
COMMENT ON CONSTRAINT like_metadata_source_id_not_null ON like_metadata_source
  IS 'copied not null';
COMMENT ON INDEX like_metadata_source_label_idx IS 'copied index';
COMMENT ON STATISTICS like_metadata_source_value_label_stats IS 'copied statistics';

CREATE TABLE like_metadata_target (LIKE like_metadata_source INCLUDING ALL);

SELECT obj_description('like_metadata_target'::regclass);
SELECT col_description('like_metadata_target'::regclass, 3);
SELECT obj_description(oid, 'pg_constraint')
  FROM pg_constraint
 WHERE conrelid = 'like_metadata_target'::regclass AND contype = 'c';
SELECT obj_description(oid, 'pg_constraint')
  FROM pg_constraint
 WHERE conrelid = 'like_metadata_target'::regclass AND contype = 'n';
SELECT obj_description(indexrelid, 'pg_class')
  FROM pg_index
 WHERE indrelid = 'like_metadata_target'::regclass AND NOT indisprimary;
SELECT obj_description(oid, 'pg_statistic_ext'), pg_get_statisticsobjdef(oid)
  FROM pg_statistic_ext
 WHERE stxrelid = 'like_metadata_target'::regclass;

CREATE TABLE like_metadata_target_excluded (
  LIKE like_metadata_source INCLUDING ALL EXCLUDING COMMENTS EXCLUDING STATISTICS
);
SELECT col_description('like_metadata_target_excluded'::regclass, 3);
SELECT count(*) FROM pg_statistic_ext
 WHERE stxrelid = 'like_metadata_target_excluded'::regclass;

CREATE TABLE IF NOT EXISTS like_metadata_target
  (LIKE like_metadata_source INCLUDING ALL);
SELECT count(*) FROM pg_index
 WHERE indrelid = 'like_metadata_target'::regclass;

DROP TABLE like_metadata_target_excluded;
DROP TABLE like_metadata_target;
DROP TABLE like_metadata_source;
