-- Describing a query must preserve anonymous and named record fields.
SELECT (source.q).f1,(source.q).f2,pg_typeof((source.q).f1)::text,pg_typeof((source.q).f2)::text
FROM (SELECT ROW(7::integer,'xy'::varchar(2)) AS q) AS source;
SELECT (result.keys).x,(result.keys).n
FROM (SELECT information_schema._pg_expandarray(ARRAY['x',NULL,'z']) AS keys) AS result
ORDER BY (result.keys).n;
CREATE TYPE planner_metadata_pair AS (x integer,label varchar(2));
CREATE DOMAIN planner_metadata_domain AS planner_metadata_pair;
SELECT ((ARRAY[ROW(9,'z')::planner_metadata_domain]::planner_metadata_domain[])[1]).x;
CREATE TABLE planner_metadata_target(id integer PRIMARY KEY,value integer);
INSERT INTO planner_metadata_target VALUES(1,10),(2,20);
UPDATE planner_metadata_target AS target SET value=value+1 WHERE id=1
RETURNING id,(SELECT target.value),(SELECT old.value),(SELECT new.value);
MERGE INTO planner_metadata_target AS target USING (VALUES(2,25)) AS source(id,value)
ON target.id=source.id WHEN MATCHED THEN UPDATE SET value=source.value
RETURNING WITH (OLD AS before,NEW AS after) before.id,before.value,after.value;
INSERT INTO planner_metadata_target VALUES(1,99) ON CONFLICT(id) DO UPDATE SET value=excluded.value WHERE excluded.missing>0;
SELECT length(DISTINCT 'x');
SELECT planner_metadata_target.value FROM planner_metadata_target AS hidden;
SELECT (SELECT public.planner_metadata_target.value) FROM planner_metadata_target AS hidden;
SELECT 'kept' FROM planner_metadata_target
WHERE (value<0 OR 'a_b'<>'') OR value>((-2147483648*2)+1) ORDER BY id LIMIT 1;
CREATE INDEX planner_metadata_index ON planner_metadata_target(value);
COMMENT ON INDEX planner_metadata_index IS 'original index';
BEGIN;
ALTER INDEX planner_metadata_index RENAME TO planner_metadata_retired;
CREATE INDEX planner_metadata_index ON planner_metadata_target(value);
COMMENT ON INDEX planner_metadata_index IS 'replacement index';
COMMIT;
SELECT relname,obj_description(oid,'pg_class') FROM pg_class
WHERE relname IN ('planner_metadata_index','planner_metadata_retired') ORDER BY relname;
DROP INDEX planner_metadata_retired;
SELECT obj_description('planner_metadata_index'::regclass,'pg_class');
-- Every advertised B-tree/hash class must resolve to its PostgreSQL family.
SELECT c.oid,c.opcmethod,c.opcname,f.opfname,c.opcintype,c.opcdefault,c.opckeytype
FROM pg_opclass c JOIN pg_opfamily f ON f.oid=c.opcfamily
WHERE c.oid IN (1978,1979,1981,4217,10028,10031,10044,10053) ORDER BY c.oid;
SELECT count(*) FROM pg_opclass WHERE opcmethod IN (403,405);
SELECT count(*) FROM pg_opclass c LEFT JOIN pg_opfamily f ON f.oid=c.opcfamily
WHERE c.opcmethod IN (403,405) AND f.oid IS NULL;
DROP TABLE planner_metadata_target;
DROP DOMAIN planner_metadata_domain;
DROP TYPE planner_metadata_pair;
