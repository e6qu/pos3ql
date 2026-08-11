-- The implemented ALTER INDEX form changes relation identity atomically.
DROP TABLE IF EXISTS alter_index_rows;

CREATE TABLE alter_index_rows (id integer, value text);
CREATE INDEX alter_index_old ON alter_index_rows (value);
COMMENT ON INDEX alter_index_old IS 'renamed index comment';
BEGIN;
ALTER INDEX alter_index_old RENAME TO alter_index_new;
SELECT indexname FROM pg_indexes
 WHERE tablename = 'alter_index_rows'
 ORDER BY indexname;
ROLLBACK;
SELECT indexname FROM pg_indexes
 WHERE tablename = 'alter_index_rows'
 ORDER BY indexname;
ALTER INDEX alter_index_old RENAME TO alter_index_new;
SELECT obj_description('alter_index_new'::regclass);
REINDEX INDEX alter_index_new;
DROP INDEX alter_index_new;
DROP TABLE alter_index_rows;
