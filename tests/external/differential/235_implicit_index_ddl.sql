CREATE ROLE implicit_index_stranger;
CREATE TABLE implicit_index_rows(id integer PRIMARY KEY,code integer UNIQUE,span int4range,
  CONSTRAINT implicit_index_exclusion EXCLUDE USING gist(span WITH &&));
INSERT INTO implicit_index_rows VALUES(1,10,'[1,2)'),(2,20,'[3,4)');
COMMENT ON CONSTRAINT implicit_index_rows_pkey ON implicit_index_rows IS 'primary constraint';
COMMENT ON INDEX implicit_index_rows_pkey IS 'primary index';
CREATE TABLE implicit_index_before AS SELECT oid,conindid FROM pg_constraint
WHERE conrelid='implicit_index_rows'::regclass AND contype IN ('p','u','x');
SET ROLE implicit_index_stranger;
ALTER INDEX implicit_index_rows_pkey RENAME TO forbidden_index_name;
RESET ROLE;
BEGIN;
ALTER INDEX implicit_index_rows_pkey RENAME TO implicit_index_temporary;
SELECT c.oid=b.oid,c.conindid=b.conindid,obj_description(c.oid,'pg_constraint'),obj_description(c.conindid,'pg_class')
FROM pg_constraint c JOIN implicit_index_before b ON c.oid=b.oid WHERE c.conname='implicit_index_temporary';
ROLLBACK;
SELECT c.oid=b.oid,c.conindid=b.conindid,obj_description(c.oid,'pg_constraint'),obj_description(c.conindid,'pg_class')
FROM pg_constraint c JOIN implicit_index_before b ON c.oid=b.oid WHERE c.conname='implicit_index_rows_pkey';
ALTER INDEX implicit_index_rows_pkey SET(fillfactor=77);
SELECT reloptions::text FROM pg_class WHERE relname='implicit_index_rows_pkey';
ALTER INDEX implicit_index_rows_pkey RENAME TO implicit_index_primary_done;
ALTER INDEX implicit_index_rows_code_key RENAME TO implicit_index_unique_done;
ALTER INDEX implicit_index_exclusion RENAME TO implicit_index_exclusion_done;
SELECT c.conname,c.oid=b.oid,c.conindid=b.conindid,obj_description(c.oid,'pg_constraint'),obj_description(c.conindid,'pg_class')
FROM pg_constraint c JOIN implicit_index_before b ON c.oid=b.oid ORDER BY c.conname;
SELECT count(*),count(DISTINCT i.indexrelid) FROM pg_index i WHERE i.indrelid='implicit_index_rows'::regclass;
DROP TABLE implicit_index_rows,implicit_index_before;
DROP ROLE implicit_index_stranger;
