CREATE TABLE brin_index_error_rows(id integer, label text);
CREATE UNIQUE INDEX brin_index_error_unique
    ON brin_index_error_rows USING brin (id);
CREATE INDEX brin_index_error_order
    ON brin_index_error_rows USING brin (id DESC);
CREATE INDEX brin_index_error_null_order
    ON brin_index_error_rows USING brin (id NULLS FIRST);
CREATE INDEX brin_index_error_include
    ON brin_index_error_rows USING brin (id) INCLUDE (label);
CREATE INDEX brin_index_error_option
    ON brin_index_error_rows USING brin (id) WITH (fillfactor = 80);
CREATE INDEX brin_index_error_class
    ON brin_index_error_rows USING brin (label int4_minmax_ops);
CREATE INDEX brin_index_error_cluster
    ON brin_index_error_rows USING brin (id);
ALTER TABLE brin_index_error_rows CLUSTER ON brin_index_error_cluster;
CLUSTER brin_index_error_rows USING brin_index_error_cluster;
DROP TABLE brin_index_error_rows;
