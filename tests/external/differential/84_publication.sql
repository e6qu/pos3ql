-- PostgreSQL 18.4 publication DDL accepted by the object-native logical stream.

CREATE TABLE publication_source (id integer PRIMARY KEY, value text);
CREATE TABLE publication_second (id integer PRIMARY KEY);

CREATE PUBLICATION publication_changes
  FOR TABLE publication_source, publication_second
  WITH (publish = 'insert, update, delete');
CREATE PUBLICATION publication_all FOR ALL TABLES;

DROP PUBLICATION publication_changes, publication_all;
DROP PUBLICATION IF EXISTS publication_missing;
DROP TABLE publication_second;
DROP TABLE publication_source;
