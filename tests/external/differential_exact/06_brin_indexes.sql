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
CREATE INDEX brin_index_error_values_low
    ON brin_index_error_rows USING brin
    (id int4_minmax_multi_ops (values_per_range=7));
CREATE INDEX brin_index_error_rate_high
    ON brin_index_error_rows USING brin
    (label text_bloom_ops (false_positive_rate=0.3));
CREATE INDEX brin_index_error_no_options
    ON brin_index_error_rows USING brin
    (id int4_minmax_ops (values_per_range=8));
CREATE INDEX brin_index_error_wrong_multi_option
    ON brin_index_error_rows USING brin
    (id int4_minmax_multi_ops (n_distinct_per_range=4));
CREATE INDEX brin_index_error_wrong_bloom_option
    ON brin_index_error_rows USING brin
    (label text_bloom_ops (values_per_range=8));
CREATE INDEX brin_index_error_hash_options
    ON brin_index_error_rows USING hash
    (id int4_ops (values_per_range=8));
CREATE INDEX brin_index_error_cluster
    ON brin_index_error_rows USING brin (id);
ALTER TABLE brin_index_error_rows CLUSTER ON brin_index_error_cluster;
CLUSTER brin_index_error_rows USING brin_index_error_cluster;
SELECT brin_summarize_range('brin_index_error_cluster'::regclass, -1);
SELECT brin_summarize_range('brin_index_error_rows'::regclass, 0);
CREATE TABLE brin_index_error_partitioned(id integer) PARTITION BY RANGE (id);
CREATE TABLE brin_index_error_partition PARTITION OF brin_index_error_partitioned
    FOR VALUES FROM (0) TO (10);
CREATE INDEX brin_index_error_partitioned_idx
    ON brin_index_error_partitioned USING brin (id);
SELECT brin_summarize_new_values('brin_index_error_partitioned_idx'::regclass);
DROP TABLE brin_index_error_partitioned;
DROP TABLE brin_index_error_rows;
