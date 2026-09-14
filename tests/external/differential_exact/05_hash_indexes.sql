CREATE TABLE hash_index_error_rows(id integer, label text, cursor_value refcursor);
CREATE UNIQUE INDEX hash_index_error_unique
    ON hash_index_error_rows USING hash (id);
CREATE INDEX hash_index_error_multi
    ON hash_index_error_rows USING hash (id, label);
CREATE INDEX hash_index_error_order
    ON hash_index_error_rows USING hash (id DESC);
CREATE INDEX hash_index_error_null_order
    ON hash_index_error_rows USING hash (id NULLS FIRST);
CREATE INDEX hash_index_error_include
    ON hash_index_error_rows USING hash (id) INCLUDE (label);
CREATE INDEX hash_index_error_option
    ON hash_index_error_rows USING hash (id) WITH (deduplicate_items = off);
CREATE INDEX hash_index_error_refcursor
    ON hash_index_error_rows USING hash (cursor_value);
CREATE INDEX hash_index_error_cluster
    ON hash_index_error_rows USING hash (id);
ALTER TABLE hash_index_error_rows CLUSTER ON hash_index_error_cluster;
CLUSTER hash_index_error_rows USING hash_index_error_cluster;
DROP TABLE hash_index_error_rows;
