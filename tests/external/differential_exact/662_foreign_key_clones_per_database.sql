-- Foreign key clones belong to one database: DDL in one database neither
-- edits nor locks the same-named tables and keys of another.
\set origin :DBNAME
\set QUIET on
CREATE DATABASE fkc_other;
\c fkc_other
CREATE TABLE pk (a int PRIMARY KEY) PARTITION BY RANGE (a);
CREATE TABLE pk1 PARTITION OF pk FOR VALUES FROM (0) TO (10);
CREATE TABLE pc (a int REFERENCES pk);
\c :origin
\set QUIET off
CREATE TABLE unrelated (a int);
CREATE TABLE pk (a int PRIMARY KEY) PARTITION BY RANGE (a);
CREATE TABLE pk1 PARTITION OF pk FOR VALUES FROM (0) TO (10);
CREATE TABLE pk2 PARTITION OF pk FOR VALUES FROM (10) TO (20);
CREATE TABLE pc (a int REFERENCES pk);
-- A referenced partition holds its clone's action triggers.
SELECT relname, relhastriggers FROM pg_class
WHERE relname IN ('pk', 'pk1', 'pk2', 'pc', 'unrelated') ORDER BY 1;
SELECT conname, conrelid::regclass, confrelid::regclass,
  (SELECT p.conname FROM pg_constraint p WHERE p.oid = c.conparentid) AS parent
FROM pg_constraint c WHERE contype = 'f' ORDER BY conname;
\d pk1
\c fkc_other
SELECT conname, conrelid::regclass, confrelid::regclass,
  (SELECT p.conname FROM pg_constraint p WHERE p.oid = c.conparentid) AS parent
FROM pg_constraint c WHERE contype = 'f' ORDER BY conname;
INSERT INTO pk VALUES (1);
INSERT INTO pc VALUES (1);
DELETE FROM pk WHERE a = 1;
\c :origin
ALTER TABLE pk DETACH PARTITION pk2;
SELECT conname, confrelid::regclass FROM pg_constraint WHERE contype = 'f' ORDER BY conname;
\set QUIET on
DROP TABLE pc, pk, pk2, unrelated;
DROP DATABASE fkc_other;
