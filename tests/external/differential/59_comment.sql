-- COMMENT ON, matching PostgreSQL 18. A comment attaches text to a database
-- object (relation, column, schema, or type); IS NULL removes it; obj_description /
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
DROP FUNCTION IF EXISTS cmt_fn(integer);
DROP PROCEDURE IF EXISTS cmt_proc(integer);
DROP AGGREGATE IF EXISTS cmt_sum(integer);
DROP FUNCTION IF EXISTS cmt_sum_state(bigint, integer);
DROP TYPE IF EXISTS cmt_enum;
DROP DOMAIN IF EXISTS cmt_domain;
DROP SCHEMA IF EXISTS cmt_ns CASCADE;

CREATE TABLE cmt_t (id int PRIMARY KEY, a text);
CREATE INDEX cmt_idx ON cmt_t (a);
CREATE VIEW cmt_v AS SELECT id FROM cmt_t;
CREATE MATERIALIZED VIEW cmt_mv AS SELECT id FROM cmt_t;
CREATE SEQUENCE cmt_s;
CREATE SCHEMA cmt_ns;
CREATE TYPE cmt_enum AS ENUM ('a', 'b');
CREATE DOMAIN cmt_domain AS int CHECK (VALUE > 0);
CREATE FUNCTION cmt_fn(value integer) RETURNS integer LANGUAGE SQL AS 'SELECT $1';
CREATE PROCEDURE cmt_proc(value integer) LANGUAGE SQL AS 'SELECT value';
CREATE FUNCTION cmt_sum_state(state bigint, value integer) RETURNS bigint LANGUAGE SQL
  AS 'SELECT coalesce(state, 0) + coalesce(value, 0)';
CREATE AGGREGATE cmt_sum(integer) (SFUNC = cmt_sum_state, STYPE = bigint);

-- Set comments on every object kind that carries an OID here.
COMMENT ON TABLE cmt_t IS 'cmttest table';
COMMENT ON COLUMN cmt_t.a IS 'cmttest column a';
COMMENT ON COLUMN cmt_v.id IS 'cmttest view column';
COMMENT ON INDEX cmt_idx IS 'cmttest index';
COMMENT ON VIEW cmt_v IS 'cmttest view';
COMMENT ON MATERIALIZED VIEW cmt_mv IS 'cmttest matview';
COMMENT ON SEQUENCE cmt_s IS 'cmttest sequence';
COMMENT ON SCHEMA cmt_ns IS 'cmttest schema';
COMMENT ON TYPE cmt_enum IS 'cmttest enum type';
COMMENT ON DOMAIN cmt_domain IS 'cmttest domain type';
COMMENT ON TYPE integer IS 'cmttest builtin type';
COMMENT ON TYPE cmt_t IS 'cmttest composite type';
COMMENT ON TYPE cmt_v IS 'cmttest view row type';
COMMENT ON TYPE cmt_mv IS 'cmttest matview row type';
COMMENT ON TYPE regclass IS 'cmttest regclass type';
COMMENT ON TYPE integer[] IS 'cmttest array type';
COMMENT ON FUNCTION cmt_fn(integer) IS 'cmttest function';
COMMENT ON PROCEDURE cmt_proc(integer) IS 'cmttest procedure';
COMMENT ON AGGREGATE cmt_sum(integer) IS 'cmttest aggregate';
COMMENT ON ROUTINE cmt_fn(integer) IS 'cmttest routine';

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
SELECT col_description('cmt_v'::regclass, 1) AS view_column_comment;
SELECT obj_description(oid, 'pg_type') AS type_comment
  FROM pg_type
 WHERE typname IN ('cmt_enum', 'cmt_domain', 'cmt_t', 'cmt_v', 'cmt_mv', 'int4')
 ORDER BY typname;
SELECT obj_description(2205, 'pg_type') AS regclass_type_comment,
       obj_description(1007, 'pg_type') AS array_type_comment;
SELECT proname, obj_description(oid, 'pg_proc') AS routine_comment
  FROM pg_proc
 WHERE proname IN ('cmt_fn', 'cmt_proc', 'cmt_sum')
 ORDER BY proname;

-- CREATE OR REPLACE preserves the view object's relation, column, and
-- composite-type comments.
CREATE OR REPLACE VIEW cmt_v AS SELECT id FROM cmt_t WHERE id >= 0;
SELECT obj_description('cmt_v'::regclass) AS replaced_view_comment,
       col_description('cmt_v'::regclass, 1) AS replaced_view_column_comment;
SELECT obj_description(oid, 'pg_type') AS replaced_view_type_comment
  FROM pg_type WHERE typname = 'cmt_v';

-- pg_description lists every relation, column, schema, and type comment.
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
COMMENT ON COLUMN cmt_v.nope IS 'x';
COMMENT ON COLUMN cmt_s.last_value IS 'x';
COMMENT ON COLUMN cmt_idx.a IS 'x';
COMMENT ON SCHEMA cmt_missing_ns IS 'x';
COMMENT ON TYPE cmt_missing_type IS 'x';
COMMENT ON TYPE pg_catalog.integer IS 'x';
COMMENT ON TYPE serial IS 'x';
COMMENT ON DOMAIN cmt_enum IS 'x';
COMMENT ON DOMAIN cmt_t IS 'x';

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

-- pg_description sees this transaction's own overlay, and rollback restores
-- the committed type comment.
BEGIN;
COMMENT ON TYPE cmt_enum IS 'cmttest doomed enum type';
SELECT description
  FROM pg_description d
  JOIN pg_type t ON t.oid = d.objoid
 WHERE t.typname = 'cmt_enum';
ROLLBACK;
SELECT obj_description(oid, 'pg_type') AS enum_after_rollback
  FROM pg_type WHERE typname = 'cmt_enum';

-- Dropping a user-defined type removes its comment; a same-named replacement
-- never inherits stale metadata.
DROP TYPE cmt_enum;
CREATE TYPE cmt_enum AS ENUM ('new');
SELECT obj_description(oid, 'pg_type') AS enum_after_recreate
  FROM pg_type WHERE typname = 'cmt_enum';

COMMENT ON TYPE integer IS NULL;
COMMENT ON TYPE regclass IS NULL;
COMMENT ON TYPE integer[] IS NULL;
DROP FUNCTION cmt_fn(integer);
CREATE FUNCTION cmt_fn(value integer) RETURNS integer LANGUAGE SQL AS 'SELECT $1';
SELECT obj_description(oid, 'pg_proc') AS recreated_function_comment
  FROM pg_proc WHERE proname = 'cmt_fn';
DROP FUNCTION cmt_fn(integer);
DROP PROCEDURE cmt_proc(integer);
DROP AGGREGATE cmt_sum(integer);
DROP FUNCTION cmt_sum_state(bigint, integer);
DROP TYPE cmt_enum;
DROP DOMAIN cmt_domain;
DROP MATERIALIZED VIEW cmt_mv;
DROP VIEW cmt_v;
DROP TABLE cmt_t;
DROP SEQUENCE cmt_s;
DROP SCHEMA cmt_ns CASCADE;
