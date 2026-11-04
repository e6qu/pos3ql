CREATE TABLE nested_input_rules(target tsquery,replacement tsquery);
INSERT INTO nested_input_rules VALUES('a','c'),('b','d');
SELECT ts_rewrite('a & b'::tsquery,'SELECT target,replacement FROM nested_input_rules ORDER BY target');
SELECT count(*),min(ts_rewrite('a & b'::tsquery,'SELECT target,replacement FROM nested_input_rules ORDER BY target')::text)
FROM generate_series(1,64);
SELECT ts_rewrite('a'::tsquery,'SELECT target FROM nested_input_rules');
SELECT ts_rewrite('a'::tsquery,'SELECT target FROM nested_input_rules WHERE false');
SELECT ts_rewrite('a'::tsquery,'SELECT 1,2');
SELECT ts_rewrite('a'::tsquery,'SELECT NULL::tsquery,NULL::tsquery');
SELECT ts_rewrite('a & b'::tsquery,'SELECT target,replacement FROM nested_input_rules ORDER BY target');
SELECT (SELECT "a.b".value);
CREATE TABLE nested_input_rows(id integer,label varchar(3));
INSERT INTO nested_input_rows VALUES(2,'two'),(1,'one');
SELECT substring('banana' FROM -3 FOR -2),1/0 FROM nested_input_rows WHERE false;
SELECT 1/0,substring('banana' FROM -3 FOR -2) FROM nested_input_rows WHERE false;
SELECT CASE WHEN true THEN 'kept' ELSE substring('banana' FROM -3 FOR -2) END;
SELECT ((source).r).id,((source).r).label,pg_typeof(((source).r).label)::text
FROM (SELECT nested_input_rows AS r FROM nested_input_rows) AS source ORDER BY 1;
SELECT ((source).r).* FROM (SELECT nested_input_rows AS r FROM nested_input_rows) AS source ORDER BY 1;
CREATE TABLE renamed_key_rows(id integer,value integer,CONSTRAINT renamed_key UNIQUE(id,value));
COMMENT ON CONSTRAINT renamed_key ON renamed_key_rows IS 'key comment';
COMMENT ON INDEX renamed_key IS 'index comment';
CREATE TABLE renamed_key_before AS SELECT oid,conindid FROM pg_constraint WHERE conname='renamed_key';
BEGIN;
ALTER INDEX renamed_key RENAME TO renamed_key_temporary;
SELECT c.oid=b.oid,c.conindid=b.conindid,obj_description(c.oid,'pg_constraint'),obj_description(c.conindid,'pg_class')
FROM pg_constraint c CROSS JOIN renamed_key_before b WHERE c.conname='renamed_key_temporary';
ROLLBACK;
SELECT c.oid=b.oid,c.conindid=b.conindid,obj_description(c.oid,'pg_constraint'),obj_description(c.conindid,'pg_class')
FROM pg_constraint c CROSS JOIN renamed_key_before b WHERE c.conname='renamed_key';
ALTER INDEX renamed_key RENAME TO renamed_key_final;
SELECT c.oid=b.oid,c.conindid=b.conindid,obj_description(c.oid,'pg_constraint'),obj_description(c.conindid,'pg_class')
FROM pg_constraint c CROSS JOIN renamed_key_before b WHERE c.conname='renamed_key_final';
CREATE TABLE nested_partition_rows(id integer PRIMARY KEY) PARTITION BY RANGE(id);
CREATE TABLE nested_partition_low PARTITION OF nested_partition_rows FOR VALUES FROM(0) TO(10);
CREATE TABLE nested_partition_high PARTITION OF nested_partition_rows FOR VALUES FROM(10) TO(20);
SELECT count(*) FILTER(WHERE i.relkind='I'),count(*) FILTER(WHERE i.relkind='i')
FROM pg_constraint c JOIN pg_class i ON i.oid=c.conindid
WHERE c.contype='p' AND c.conrelid IN ('nested_partition_rows'::regclass,'nested_partition_low'::regclass,'nested_partition_high'::regclass);
REINDEX INDEX CONCURRENTLY nested_partition_rows_pkey;
SELECT count(*) FILTER(WHERE i.relkind='I'),count(*) FILTER(WHERE i.relkind='i')
FROM pg_constraint c JOIN pg_class i ON i.oid=c.conindid
WHERE c.contype='p' AND c.conrelid IN ('nested_partition_rows'::regclass,'nested_partition_low'::regclass,'nested_partition_high'::regclass);
DROP TABLE nested_input_rules,nested_input_rows,renamed_key_rows,renamed_key_before,nested_partition_rows;
