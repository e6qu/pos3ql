-- Text search parser set functions in a select list return records.
SELECT ts_parse('default', 'a b');
SELECT ts_token_type('default') LIMIT 3;
SELECT ts_debug('simple', 'Hi there');
SELECT count(*) FROM (SELECT ts_token_type('default')) s;
-- Table functions with their own materialization joined beside a table, as
-- psql's \d does with pg_partition_ancestors; the forced-spill run streams
-- this join through an external run.
CREATE TABLE tsf_pk (id int PRIMARY KEY);
CREATE TABLE tsf_fp (id int, ref int REFERENCES tsf_pk) PARTITION BY LIST (id);
CREATE TABLE tsf_fp_1 PARTITION OF tsf_fp FOR VALUES IN (1);
SELECT conname, relid::regclass FROM pg_constraint r, pg_partition_ancestors('tsf_fp_1')
WHERE conrelid = relid AND contype = 'f' ORDER BY 1, 2;
SELECT t.relid::regclass, t.level FROM pg_class c, pg_partition_tree(c.oid) t
WHERE c.relname = 'tsf_fp' ORDER BY 2, 1;
SELECT count(*) FROM pg_class c, pg_snapshot_xip('10:20:10,14,15'::pg_snapshot) WHERE c.relname = 'tsf_pk';
SELECT count(*) FROM pg_class c, ts_token_type('default') WHERE c.relname = 'tsf_pk';
\d tsf_fp_1
DROP TABLE tsf_fp, tsf_pk;
