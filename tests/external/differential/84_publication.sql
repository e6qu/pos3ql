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

CREATE PUBLICATION publication_changes
  FOR TABLE publication_source (id, value), publication_second
  WITH (publish = 'insert, update, delete');
CREATE PUBLICATION publication_all FOR ALL TABLES;
CREATE PUBLICATION publication_empty;
CREATE PUBLICATION publication_generated_changes FOR TABLE publication_generated
  WITH (publish_generated_columns = 'stored');
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
DROP PUBLICATION publication_changes, publication_all, publication_empty_renamed,
  publication_schema_changes, publication_generated_changes;
DROP TABLE publication_generated;
DROP TABLE publication_third;
DROP PUBLICATION IF EXISTS publication_missing;
DROP TABLE publication_second;
DROP TABLE publication_source;
DROP SCHEMA publication_schema CASCADE;
DROP ROLE publication_owner_target;
