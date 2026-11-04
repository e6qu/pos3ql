CREATE ROLE index_parent_owner;
CREATE TABLE index_parent_rows(id integer PRIMARY KEY, payload text);
INSERT INTO index_parent_rows VALUES(1,'value');
CREATE INDEX index_parent_explicit ON index_parent_rows(payload);
REINDEX TABLE CONCURRENTLY index_parent_rows;
SELECT 'initial', bool_and(t.relowner = r.oid), bool_and(i.relowner = r.oid)
  FROM pg_class t JOIN pg_index x ON x.indrelid = t.oid OR x.indrelid = t.reltoastrelid
  JOIN pg_class i ON i.oid = x.indexrelid JOIN pg_roles r ON r.rolname = 'postgres'
 WHERE t.oid = 'index_parent_rows'::regclass;
BEGIN;
ALTER TABLE index_parent_rows OWNER TO index_parent_owner;
SELECT 'pending', bool_and(t.relowner = r.oid), bool_and(i.relowner = r.oid)
  FROM pg_class t JOIN pg_index x ON x.indrelid = t.oid OR x.indrelid = t.reltoastrelid
  JOIN pg_class i ON i.oid = x.indexrelid JOIN pg_roles r ON r.rolname = 'index_parent_owner'
 WHERE t.oid = 'index_parent_rows'::regclass;
ROLLBACK;
SELECT 'rolled-back', bool_and(t.relowner = r.oid), bool_and(i.relowner = r.oid)
  FROM pg_class t JOIN pg_index x ON x.indrelid = t.oid OR x.indrelid = t.reltoastrelid
  JOIN pg_class i ON i.oid = x.indexrelid JOIN pg_roles r ON r.rolname = 'postgres'
 WHERE t.oid = 'index_parent_rows'::regclass;
ALTER TABLE index_parent_rows OWNER TO index_parent_owner;
SELECT 'committed', bool_and(t.relowner = r.oid), bool_and(i.relowner = r.oid)
  FROM pg_class t JOIN pg_index x ON x.indrelid = t.oid OR x.indrelid = t.reltoastrelid
  JOIN pg_class i ON i.oid = x.indexrelid JOIN pg_roles r ON r.rolname = 'index_parent_owner'
 WHERE t.oid = 'index_parent_rows'::regclass;
CREATE MATERIALIZED VIEW index_parent_view AS SELECT id,payload FROM index_parent_rows;
CREATE INDEX index_parent_view_explicit ON index_parent_view(id);
ALTER MATERIALIZED VIEW index_parent_view OWNER TO index_parent_owner;
SELECT 'view-committed', bool_and(t.relowner = r.oid), bool_and(i.relowner = r.oid)
  FROM pg_class t JOIN pg_index x ON x.indrelid = t.oid OR x.indrelid = t.reltoastrelid
  JOIN pg_class i ON i.oid = x.indexrelid JOIN pg_roles r ON r.rolname = 'index_parent_owner'
 WHERE t.oid = 'index_parent_view'::regclass;
BEGIN;
ALTER MATERIALIZED VIEW index_parent_view OWNER TO postgres;
SELECT 'view-pending', bool_and(t.relowner = r.oid), bool_and(i.relowner = r.oid)
  FROM pg_class t JOIN pg_index x ON x.indrelid = t.oid OR x.indrelid = t.reltoastrelid
  JOIN pg_class i ON i.oid = x.indexrelid JOIN pg_roles r ON r.rolname = 'postgres'
 WHERE t.oid = 'index_parent_view'::regclass;
ROLLBACK;
SELECT 'view-rolled-back', bool_and(t.relowner = r.oid), bool_and(i.relowner = r.oid)
  FROM pg_class t JOIN pg_index x ON x.indrelid = t.oid OR x.indrelid = t.reltoastrelid
  JOIN pg_class i ON i.oid = x.indexrelid JOIN pg_roles r ON r.rolname = 'index_parent_owner'
 WHERE t.oid = 'index_parent_view'::regclass;
DROP MATERIALIZED VIEW index_parent_view;
DROP TABLE index_parent_rows;
DROP ROLE index_parent_owner;
