CREATE ROLE reindex_owner;
CREATE ROLE reindex_maintainer;
CREATE TABLE reindex_owned(id integer PRIMARY KEY, payload text);
INSERT INTO reindex_owned VALUES(1,'value');
ALTER TABLE reindex_owned OWNER TO reindex_owner;
GRANT MAINTAIN ON reindex_owned TO reindex_maintainer;
GRANT SELECT(id) ON reindex_owned TO reindex_maintainer;
CREATE INDEX reindex_superuser ON reindex_owned(payload);
SELECT count(*) = 2, bool_and(c.relowner = r.oid)
  FROM pg_class c JOIN pg_index i ON i.indexrelid = c.oid
  JOIN pg_roles r ON r.rolname = 'reindex_owner'
 WHERE i.indrelid = 'reindex_owned'::regclass;
SET ROLE reindex_maintainer;
REINDEX TABLE CONCURRENTLY reindex_owned;
SELECT has_table_privilege('reindex_maintainer','reindex_owned','MAINTAIN'),
       has_column_privilege('reindex_maintainer','reindex_owned','id','SELECT');
RESET ROLE;
SELECT count(*) = 2, bool_and(c.relowner = r.oid)
  FROM pg_class c JOIN pg_index i ON i.indexrelid = c.oid
  JOIN pg_roles r ON r.rolname = 'reindex_owner'
 WHERE i.indrelid = 'reindex_owned'::regclass;
SELECT count(*) = 1, bool_and(c.relowner = r.oid)
  FROM pg_class t JOIN pg_index i ON i.indrelid = t.reltoastrelid
  JOIN pg_class c ON c.oid = i.indexrelid
  JOIN pg_roles r ON r.rolname = 'reindex_owner'
 WHERE t.oid = 'reindex_owned'::regclass;
DROP TABLE reindex_owned;
DROP ROLE reindex_maintainer, reindex_owner;
