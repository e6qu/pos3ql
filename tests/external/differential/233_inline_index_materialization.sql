CREATE TABLE inline_key_materialization(id integer PRIMARY KEY,value integer UNIQUE,label text);
INSERT INTO inline_key_materialization VALUES(1,10,'one'),(2,20,'two');
SELECT 'created',count(*),count(DISTINCT c.oid),count(*) FILTER (WHERE i.indisprimary),count(*) FILTER (WHERE i.indisunique)
FROM pg_class c JOIN pg_index i ON i.indexrelid=c.oid WHERE i.indrelid='inline_key_materialization'::regclass;
REINDEX (TABLESPACE pg_default) TABLE inline_key_materialization;
SELECT 'materialized',count(*),count(DISTINCT c.oid),count(*) FILTER (WHERE i.indisprimary),count(*) FILTER (WHERE i.indisunique)
FROM pg_class c JOIN pg_index i ON i.indexrelid=c.oid WHERE i.indrelid='inline_key_materialization'::regclass;
SELECT c.relname,i.indisprimary,i.indisunique,i.indimmediate FROM pg_class c JOIN pg_index i ON i.indexrelid=c.oid
WHERE i.indrelid='inline_key_materialization'::regclass ORDER BY c.relname;
SELECT count(*),count(DISTINCT conindid) FROM pg_constraint
WHERE conrelid='inline_key_materialization'::regclass AND contype IN ('p','u');
REINDEX TABLE CONCURRENTLY inline_key_materialization;
SELECT 'replaced',count(*),count(DISTINCT c.oid),count(*) FILTER (WHERE i.indisprimary),count(*) FILTER (WHERE i.indisunique)
FROM pg_class c JOIN pg_index i ON i.indexrelid=c.oid WHERE i.indrelid='inline_key_materialization'::regclass;
SELECT c.relname,i.indisprimary,i.indisunique,i.indimmediate FROM pg_class c JOIN pg_index i ON i.indexrelid=c.oid
WHERE i.indrelid='inline_key_materialization'::regclass ORDER BY c.relname;
SELECT count(*) FROM pg_class WHERE relname LIKE 'inline_key_materialization%_ccnew%' OR relname LIKE 'inline_key_materialization%_ccold%';
SELECT id,value,label FROM inline_key_materialization ORDER BY id;
DROP TABLE inline_key_materialization;
