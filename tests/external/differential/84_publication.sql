-- PostgreSQL 18.4 publication DDL accepted by the object-native logical stream.

CREATE TABLE publication_source (id integer PRIMARY KEY, value text);
CREATE TABLE publication_second (id integer PRIMARY KEY);
CREATE TABLE publication_third (id integer PRIMARY KEY);

CREATE PUBLICATION publication_changes
  FOR TABLE publication_source, publication_second
  WITH (publish = 'insert, update, delete');
CREATE PUBLICATION publication_all FOR ALL TABLES;
CREATE PUBLICATION publication_empty;
SELECT count(*) FROM pg_publication_rel rel
  JOIN pg_publication pub ON pub.oid = rel.prpubid
 WHERE pub.pubname = 'publication_empty';
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

DROP PUBLICATION publication_changes, publication_all, publication_empty;
DROP TABLE publication_third;
DROP PUBLICATION IF EXISTS publication_missing;
DROP TABLE publication_second;
DROP TABLE publication_source;
