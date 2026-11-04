CREATE TABLE reindex_toast_identity(id integer PRIMARY KEY, body text);
INSERT INTO reindex_toast_identity VALUES(1,'value');
CREATE TABLE reindex_toast_before AS
  SELECT t.oid AS toast_oid, i.indexrelid AS index_oid
    FROM pg_class t JOIN pg_index i ON i.indrelid = t.oid
   WHERE t.oid = (SELECT reltoastrelid FROM pg_class WHERE oid = 'reindex_toast_identity'::regclass);
SELECT 'before', i.indisunique, i.indisprimary, i.indisvalid, i.indisready, i.indislive,
       i.indimmediate, i.indkey::text, i.indnkeyatts, i.indnatts, i.indcollation::text,
       (SELECT string_agg(o.opcname,',' ORDER BY u.n)
          FROM unnest(i.indclass) WITH ORDINALITY u(oid,n) JOIN pg_opclass o ON o.oid = u.oid)
  FROM pg_index i JOIN reindex_toast_before b ON i.indexrelid = b.index_oid;
SELECT 'attributes', a.attnum, a.attname, a.atttypid, a.attnotnull
  FROM pg_attribute a JOIN reindex_toast_before b ON a.attrelid = b.index_oid
 WHERE a.attnum > 0 ORDER BY a.attnum;
WITH selected AS (
  SELECT c.oid AS owner_oid, c.reltoastrelid AS toast_oid, i.indexrelid AS index_oid
    FROM pg_class c JOIN pg_index i ON i.indrelid = c.reltoastrelid
   WHERE c.oid = 'reindex_toast_identity'::regclass
)
SELECT 'before-dependencies', d.classid::regclass::text,
       CASE d.objid WHEN s.toast_oid THEN 'toast' WHEN s.index_oid THEN 'index' END,
       d.objsubid, d.refclassid::regclass::text,
       CASE d.refobjid WHEN s.owner_oid THEN 'owner' WHEN s.toast_oid THEN 'toast' END,
       d.refobjsubid, d.deptype
  FROM pg_depend d, selected s
 WHERE d.classid = 'pg_class'::regclass AND d.objid IN(s.toast_oid,s.index_oid)
 ORDER BY 3,d.objsubid,5,6,d.refobjsubid,d.deptype;
REINDEX TABLE CONCURRENTLY reindex_toast_identity;
SELECT 'after', i.indisunique, i.indisprimary, i.indisvalid, i.indisready, i.indislive,
       i.indimmediate, i.indkey::text, i.indnkeyatts, i.indnatts, i.indcollation::text,
       (SELECT string_agg(o.opcname,',' ORDER BY u.n)
          FROM unnest(i.indclass) WITH ORDINALITY u(oid,n) JOIN pg_opclass o ON o.oid = u.oid),
       i.indrelid = b.toast_oid, i.indexrelid <> b.index_oid
  FROM pg_class c JOIN pg_index i ON i.indrelid = c.reltoastrelid, reindex_toast_before b
 WHERE c.oid = 'reindex_toast_identity'::regclass;
WITH selected AS (
  SELECT c.oid AS owner_oid, c.reltoastrelid AS toast_oid, i.indexrelid AS index_oid
    FROM pg_class c JOIN pg_index i ON i.indrelid = c.reltoastrelid
   WHERE c.oid = 'reindex_toast_identity'::regclass
)
SELECT 'after-dependencies', d.classid::regclass::text,
       CASE d.objid WHEN s.toast_oid THEN 'toast' WHEN s.index_oid THEN 'index' END,
       d.objsubid, d.refclassid::regclass::text,
       CASE d.refobjid WHEN s.owner_oid THEN 'owner' WHEN s.toast_oid THEN 'toast' END,
       d.refobjsubid, d.deptype
  FROM pg_depend d, selected s
 WHERE d.classid = 'pg_class'::regclass AND d.objid IN(s.toast_oid,s.index_oid)
 ORDER BY 3,d.objsubid,5,6,d.refobjsubid,d.deptype;
SELECT count(*) = 1 FROM pg_class c JOIN pg_index i ON i.indrelid = c.reltoastrelid
 WHERE c.oid = 'reindex_toast_identity'::regclass;
DROP TABLE reindex_toast_identity, reindex_toast_before;
