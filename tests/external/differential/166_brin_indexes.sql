-- PostgreSQL 18 BRIN DDL, catalogs, bitmap-eligible execution, and cloning.
\pset null 'NULL'

CREATE TABLE brin_index_rows (
    id integer,
    category text,
    active boolean,
    payload text
);
CREATE INDEX brin_index_rows_id ON brin_index_rows USING brin (id)
    WITH (pages_per_range = 32, autosummarize = yes);
CREATE INDEX brin_index_rows_category ON brin_index_rows USING brin
    (lower(category) text_bloom_ops, id int4_minmax_multi_ops)
    WHERE active;

INSERT INTO brin_index_rows VALUES
    (1, 'Alpha', true, 'one'),
    (2, 'Beta', false, 'two'),
    (3, 'Gamma', true, 'three'),
    (4, 'Alpha', true, 'four');
SELECT id, payload FROM brin_index_rows WHERE id BETWEEN 2 AND 3 ORDER BY id;
CHECKPOINT;
SELECT payload FROM brin_index_rows WHERE id = 3;
PREPARE brin_index_lookup(integer) AS
    SELECT payload FROM brin_index_rows WHERE id = $1;
EXECUTE brin_index_lookup(3);
DEALLOCATE brin_index_lookup;
SELECT id FROM brin_index_rows
 WHERE active AND lower(category) = 'alpha' ORDER BY id;
UPDATE brin_index_rows SET payload = 'two-updated' WHERE id = 2
 RETURNING id, payload;
DELETE FROM brin_index_rows WHERE id = 1 RETURNING id;

ALTER INDEX brin_index_rows_id
    SET (pages_per_range = 64, autosummarize = 0);
ALTER INDEX brin_index_rows_id RESET (autosummarize);
SELECT indexname, indexdef
  FROM pg_indexes
 WHERE tablename = 'brin_index_rows'
 ORDER BY indexname;
SELECT index_relation.relname, access_method.amname,
       index_catalog.indclass::text, index_relation.reloptions::text
  FROM pg_index AS index_catalog
  JOIN pg_class AS index_relation
    ON index_relation.oid = index_catalog.indexrelid
  JOIN pg_am AS access_method ON access_method.oid = index_relation.relam
 WHERE index_relation.relname LIKE 'brin_index_rows_%'
 ORDER BY index_relation.relname;

CREATE TABLE brin_index_clone
    (LIKE brin_index_rows INCLUDING INDEXES);
SELECT access_method.amname, index_catalog.indclass::text,
       index_relation.reloptions::text
  FROM pg_index AS index_catalog
  JOIN pg_class AS index_relation
    ON index_relation.oid = index_catalog.indexrelid
  JOIN pg_am AS access_method ON access_method.oid = index_relation.relam
 WHERE index_catalog.indrelid = 'brin_index_clone'::regclass
 ORDER BY index_relation.relname;

CREATE TABLE brin_index_partitioned (id integer, payload text)
    PARTITION BY RANGE (id);
CREATE TABLE brin_index_partition_low PARTITION OF brin_index_partitioned
    FOR VALUES FROM (0) TO (10);
CREATE TABLE brin_index_partition_high PARTITION OF brin_index_partitioned
    FOR VALUES FROM (10) TO (20);
CREATE INDEX brin_index_partitioned_id
    ON brin_index_partitioned USING brin (id) WITH (pages_per_range = 16);
INSERT INTO brin_index_partitioned VALUES (4, 'low'), (14, 'high');
SELECT payload FROM brin_index_partitioned WHERE id = 14;
SELECT parent.relname, parent_method.amname, child.relname, child_method.amname
  FROM pg_inherits AS inheritance
  JOIN pg_class AS parent ON parent.oid = inheritance.inhparent
  JOIN pg_am AS parent_method ON parent_method.oid = parent.relam
  JOIN pg_class AS child ON child.oid = inheritance.inhrelid
  JOIN pg_am AS child_method ON child_method.oid = child.relam
 WHERE parent.relname = 'brin_index_partitioned_id'
 ORDER BY child.relname;

REINDEX TABLE brin_index_rows;
SELECT payload FROM brin_index_rows WHERE id = 2;

SELECT (SELECT count(*) FROM pg_opclass WHERE opcmethod = 3580),
       (SELECT count(*) FROM pg_opfamily WHERE opfmethod = 3580),
       (SELECT count(*) FROM pg_amop WHERE amopmethod = 3580),
       (SELECT count(*) FROM pg_amproc AS procedure
          JOIN pg_opfamily AS family ON family.oid = procedure.amprocfamily
         WHERE family.opfmethod = 3580);
SELECT oid, opcname, opcfamily, opcintype, opcdefault, opckeytype
  FROM pg_opclass WHERE opcmethod = 3580 ORDER BY oid;
SELECT oid, opfname, opfnamespace, opfowner
  FROM pg_opfamily WHERE opfmethod = 3580 ORDER BY oid;
SELECT oid, amopfamily, amoplefttype, amoprighttype, amopstrategy,
       amoppurpose, amopopr, amopsortfamily
  FROM pg_amop WHERE amopmethod = 3580 ORDER BY oid;
SELECT procedure.oid, procedure.amprocfamily, procedure.amproclefttype,
       procedure.amprocrighttype, procedure.amprocnum, procedure.amproc
  FROM pg_amproc AS procedure
  JOIN pg_opfamily AS family ON family.oid = procedure.amprocfamily
 WHERE family.opfmethod = 3580
 ORDER BY procedure.oid;

DROP TABLE brin_index_partitioned CASCADE;
DROP TABLE brin_index_clone;
DROP TABLE brin_index_rows;
