-- COMMENT ON, matching PostgreSQL 18. A comment attaches text to a database
-- object (relation, column or schema); IS NULL removes it; obj_description /
-- col_description / pg_description read it back. Comments are transactional:
-- a rolled-back COMMENT leaves no trace.
--
-- Comment texts carry a distinctive `cmttest` prefix so the pg_description
-- scan below never collides with PostgreSQL's own built-in object comments.
--
-- Distinctive names + drop up front (the differential corpora share a database).
DROP MATERIALIZED VIEW IF EXISTS cmt_mv;
DROP VIEW IF EXISTS cmt_v;
DROP TABLE IF EXISTS cmt_t;
DROP SEQUENCE IF EXISTS cmt_s;
DROP SCHEMA IF EXISTS cmt_ns CASCADE;

CREATE TABLE cmt_t (id int PRIMARY KEY, a text);
CREATE INDEX cmt_idx ON cmt_t (a);
CREATE VIEW cmt_v AS SELECT id FROM cmt_t;
CREATE MATERIALIZED VIEW cmt_mv AS SELECT id FROM cmt_t;
CREATE SEQUENCE cmt_s;
CREATE SCHEMA cmt_ns;

-- Set comments on every object kind that carries an OID here.
COMMENT ON TABLE cmt_t IS 'cmttest table';
COMMENT ON COLUMN cmt_t.a IS 'cmttest column a';
COMMENT ON INDEX cmt_idx IS 'cmttest index';
COMMENT ON VIEW cmt_v IS 'cmttest view';
COMMENT ON MATERIALIZED VIEW cmt_mv IS 'cmttest matview';
COMMENT ON SEQUENCE cmt_s IS 'cmttest sequence';
COMMENT ON SCHEMA cmt_ns IS 'cmttest schema';

-- Read them back through the standard helpers.
SELECT obj_description('cmt_t'::regclass) AS table_comment;
SELECT obj_description('cmt_t'::regclass, 'pg_class') AS table_comment2;
SELECT col_description('cmt_t'::regclass, 2) AS column_comment;
SELECT obj_description('cmt_idx'::regclass) AS index_comment;
SELECT obj_description('cmt_v'::regclass) AS view_comment;
SELECT obj_description('cmt_mv'::regclass) AS matview_comment;
SELECT obj_description('cmt_s'::regclass) AS sequence_comment;
SELECT obj_description(oid, 'pg_namespace') AS schema_comment
  FROM pg_namespace WHERE nspname = 'cmt_ns';

-- pg_description lists them all (schema/table/column/index/matview/sequence).
SELECT objsubid, description
  FROM pg_description
 WHERE description LIKE 'cmttest%'
 ORDER BY description, objsubid;

-- Overwrite is last-write-wins.
COMMENT ON TABLE cmt_t IS 'cmttest renamed';
SELECT obj_description('cmt_t'::regclass) AS after_overwrite;

-- IS NULL removes the comment.
COMMENT ON TABLE cmt_t IS NULL;
SELECT obj_description('cmt_t'::regclass) AS after_null;
SELECT count(*) AS remaining_table_rows
  FROM pg_description
 WHERE objoid = 'cmt_t'::regclass AND objsubid = 0;

-- Errors: missing relation, wrong object kind, missing column, missing schema.
COMMENT ON TABLE cmt_missing IS 'x';
COMMENT ON TABLE cmt_v IS 'x';
COMMENT ON VIEW cmt_t IS 'x';
COMMENT ON SEQUENCE cmt_t IS 'x';
COMMENT ON INDEX cmt_t IS 'x';
COMMENT ON COLUMN cmt_t.nope IS 'x';
COMMENT ON SCHEMA cmt_missing_ns IS 'x';

-- Transactional: a rolled-back COMMENT restores the prior committed comment.
SELECT col_description('cmt_t'::regclass, 2) AS column_before_txn;
BEGIN;
COMMENT ON COLUMN cmt_t.a IS 'cmttest doomed';
ROLLBACK;
SELECT col_description('cmt_t'::regclass, 2) AS column_after_rollback;

-- A rolled-back overwrite of the index comment restores the committed text.
BEGIN;
COMMENT ON INDEX cmt_idx IS 'cmttest doomed index';
ROLLBACK;
SELECT obj_description('cmt_idx'::regclass) AS index_after_rollback;

DROP MATERIALIZED VIEW cmt_mv;
DROP VIEW cmt_v;
DROP TABLE cmt_t;
DROP SEQUENCE cmt_s;
DROP SCHEMA cmt_ns CASCADE;
