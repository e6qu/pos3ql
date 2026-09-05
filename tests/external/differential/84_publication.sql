-- PostgreSQL 18.4 publication DDL accepted by the object-native logical stream.

CREATE TABLE publication_source (id integer PRIMARY KEY, value text);
CREATE TABLE publication_second (id integer PRIMARY KEY);
CREATE TABLE publication_third (id integer PRIMARY KEY);
CREATE TABLE publication_generated (
  id integer PRIMARY KEY,
  source text,
  derived text GENERATED ALWAYS AS (source || '-derived') STORED
);
CREATE SCHEMA publication_schema;
CREATE TABLE publication_schema.schema_selected (id integer PRIMARY KEY);
CREATE TABLE publication_union_columns (
  id integer PRIMARY KEY,
  left_value text,
  right_value text
);

CREATE PUBLICATION publication_changes
  FOR TABLE publication_source (id, value), publication_second
  WITH (publish = 'insert, update, delete');
CREATE PUBLICATION publication_all FOR ALL TABLES;
CREATE PUBLICATION publication_empty;
COMMENT ON PUBLICATION publication_empty IS 'empty publication';
SELECT obj_description(oid, 'pg_publication') FROM pg_publication
 WHERE pubname = 'publication_empty';
CREATE PUBLICATION publication_generated_changes FOR TABLE publication_generated
  WITH (publish_generated_columns = 'stored');
CREATE PUBLICATION publication_union_left
  FOR TABLE publication_union_columns (id, left_value);
CREATE PUBLICATION publication_union_right
  FOR TABLE publication_union_columns (id, right_value);
SELECT pubname, attnames::text FROM pg_publication_tables
 WHERE pubname IN ('publication_union_left', 'publication_union_right')
 ORDER BY pubname;
SELECT pubgencols FROM pg_publication WHERE pubname = 'publication_generated_changes';
ALTER PUBLICATION publication_generated_changes SET (publish_generated_columns = 'none');
SELECT pubgencols FROM pg_publication WHERE pubname = 'publication_generated_changes';
CREATE ROLE publication_owner_target;
ALTER PUBLICATION publication_empty OWNER TO publication_owner_target;
SELECT role.rolname FROM pg_publication publication
 JOIN pg_roles role ON role.oid = publication.pubowner
 WHERE publication.pubname = 'publication_empty';
ALTER PUBLICATION publication_empty RENAME TO publication_empty_renamed;
SELECT pubname FROM pg_publication WHERE pubname = 'publication_empty_renamed';
SELECT obj_description(oid, 'pg_publication') FROM pg_publication
 WHERE pubname = 'publication_empty_renamed';
CREATE PUBLICATION publication_schema_changes FOR TABLES IN SCHEMA publication_schema;
SELECT count(*) FROM pg_publication_namespace publication_namespace
  JOIN pg_publication publication ON publication.oid = publication_namespace.pnpubid
 WHERE publication.pubname = 'publication_schema_changes';
SELECT count(*) FROM pg_publication_rel rel
  JOIN pg_publication pub ON pub.oid = rel.prpubid
 WHERE pub.pubname = 'publication_empty';
SELECT prattrs::text FROM pg_publication_rel rel
 JOIN pg_publication pub ON pub.oid = rel.prpubid
 JOIN pg_class cls ON cls.oid = rel.prrelid
 WHERE pub.pubname = 'publication_changes' AND cls.relname = 'publication_source';
ALTER PUBLICATION publication_empty ADD TABLE publication_source;
SELECT count(*) FROM pg_publication_rel rel
  JOIN pg_publication pub ON pub.oid = rel.prpubid
 WHERE pub.pubname = 'publication_empty';

ALTER PUBLICATION publication_changes ADD TABLE publication_third;
ALTER PUBLICATION publication_changes SET (publish = 'insert, delete');
SELECT pubinsert, pubupdate, pubdelete, pubtruncate
  FROM pg_publication WHERE pubname = 'publication_changes';
SELECT relname
  FROM pg_publication_rel rel
  JOIN pg_class cls ON cls.oid = rel.prrelid
  JOIN pg_publication pub ON pub.oid = rel.prpubid
 WHERE pub.pubname = 'publication_changes'
 ORDER BY relname;
BEGIN;
ALTER PUBLICATION publication_changes DROP TABLE publication_second;
ALTER PUBLICATION publication_changes SET (publish = 'update');
SELECT pubinsert, pubupdate, pubdelete, pubtruncate
  FROM pg_publication WHERE pubname = 'publication_changes';
SELECT relname
  FROM pg_publication_rel rel
  JOIN pg_class cls ON cls.oid = rel.prrelid
  JOIN pg_publication pub ON pub.oid = rel.prpubid
 WHERE pub.pubname = 'publication_changes'
 ORDER BY relname;
ROLLBACK;
SELECT pubinsert, pubupdate, pubdelete, pubtruncate
  FROM pg_publication WHERE pubname = 'publication_changes';
SELECT relname
  FROM pg_publication_rel rel
  JOIN pg_class cls ON cls.oid = rel.prrelid
  JOIN pg_publication pub ON pub.oid = rel.prpubid
 WHERE pub.pubname = 'publication_changes'
 ORDER BY relname;
ALTER PUBLICATION publication_changes SET TABLE publication_source;
SELECT relname
  FROM pg_publication_rel rel
  JOIN pg_class cls ON cls.oid = rel.prrelid
  JOIN pg_publication pub ON pub.oid = rel.prpubid
 WHERE pub.pubname = 'publication_changes';

ALTER PUBLICATION publication_schema_changes DROP TABLES IN SCHEMA publication_schema;
ALTER PUBLICATION publication_schema_changes ADD TABLE publication_schema.schema_selected, TABLES IN SCHEMA publication_schema;

CREATE TABLE publication_filter_rename (id integer PRIMARY KEY);
CREATE PUBLICATION publication_filter_rename_changes
  FOR TABLE publication_filter_rename WHERE (id > 0);
ALTER TABLE publication_filter_rename RENAME COLUMN id TO event_id;
SELECT pg_get_expr(prqual, prrelid) FROM pg_publication_rel rel
  JOIN pg_publication pub ON pub.oid = rel.prpubid
 WHERE pub.pubname = 'publication_filter_rename_changes';
CREATE TABLE publication_projection_drop (
  id integer PRIMARY KEY,
  discarded text,
  retained text
);
CREATE PUBLICATION publication_projection_drop_changes
  FOR TABLE publication_projection_drop (id, retained);
ALTER TABLE publication_projection_drop DROP COLUMN discarded;
ALTER TABLE publication_projection_drop DROP COLUMN retained CASCADE;
SELECT count(*) FROM pg_publication_rel rel
  JOIN pg_publication pub ON pub.oid = rel.prpubid
 WHERE pub.pubname = 'publication_projection_drop_changes';
DROP PUBLICATION publication_filter_rename_changes, publication_projection_drop_changes;
DROP TABLE publication_filter_rename, publication_projection_drop;

DROP PUBLICATION publication_changes, publication_all, publication_empty_renamed,
  publication_schema_changes, publication_generated_changes, publication_union_left,
  publication_union_right;
DROP TABLE publication_generated;
DROP TABLE publication_third;
DROP PUBLICATION IF EXISTS publication_missing;
DROP TABLE publication_second;
DROP TABLE publication_source;
DROP TABLE publication_union_columns;
DROP SCHEMA publication_schema CASCADE;
DROP ROLE publication_owner_target;
