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
    (lower(category) text_bloom_ops
        (n_distinct_per_range='128', false_positive_rate=0.05),
     id int4_minmax_multi_ops (values_per_range=8))
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
SELECT relation.relname, attribute.attname, attribute.attoptions::text
  FROM pg_attribute AS attribute
  JOIN pg_class AS relation ON relation.oid = attribute.attrelid
 WHERE relation.relname = 'brin_index_rows_category'
   AND attribute.attnum > 0
 ORDER BY attribute.attnum;

SELECT brin_summarize_new_values('brin_index_rows_id'::regclass);
SELECT brin_desummarize_range('brin_index_rows_id'::regclass, 0);
SELECT brin_summarize_range('brin_index_rows_id'::regclass, 0);
SELECT brin_summarize_range('brin_index_rows_id'::regclass, 0);
BEGIN;
SELECT brin_desummarize_range('brin_index_rows_id'::regclass, 0);
ROLLBACK;
SELECT brin_summarize_range('brin_index_rows_id'::regclass, 0);

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

CREATE TABLE brin_inclusion_rows (
    id integer, span int4range, network inet, payload text
);
CREATE INDEX brin_inclusion_rows_values ON brin_inclusion_rows USING brin
    (span range_inclusion_ops, network inet_inclusion_ops)
    WITH (pages_per_range = 1);
INSERT INTO brin_inclusion_rows VALUES
    (1, '[1,5)'::int4range, '10.0.0.0/8'::inet, 'broad'),
    (2, '[8,14)'::int4range, '10.1.0.0/16'::inet, 'middle'),
    (3, '[20,30)'::int4range, '10.1.2.0/24'::inet, 'narrow'),
    (4, 'empty'::int4range, '192.0.2.1'::inet, 'outside');
SELECT id FROM brin_inclusion_rows
 WHERE span && '[4,10)'::int4range ORDER BY id;
SELECT id FROM brin_inclusion_rows
 WHERE network >>= '10.1.2.0/24'::inet ORDER BY id;
SELECT id FROM brin_inclusion_rows
 WHERE network <<= '10.0.0.0/8'::inet ORDER BY id;
SELECT id FROM brin_inclusion_rows
 WHERE network >> '10.1.2.0/24'::inet ORDER BY id;
SELECT id FROM brin_inclusion_rows
 WHERE network << '10.0.0.0/8'::inet ORDER BY id;
SELECT id FROM brin_inclusion_rows
 WHERE span -|- '[5,8)'::int4range ORDER BY id;
SELECT id FROM brin_inclusion_rows
 WHERE span << '[20,30)'::int4range ORDER BY id;
SELECT id FROM brin_inclusion_rows
 WHERE span &< '[6,7)'::int4range ORDER BY id;
SELECT id FROM brin_inclusion_rows
 WHERE span &> '[15,16)'::int4range ORDER BY id;
SELECT id FROM brin_inclusion_rows
 WHERE span >> '[1,5)'::int4range ORDER BY id;
SELECT id FROM brin_inclusion_rows
 WHERE network && '10.1.0.0/16'::inet ORDER BY id;
PREPARE brin_inclusion_lookup(int4range) AS
    SELECT id FROM brin_inclusion_rows WHERE span && $1 ORDER BY id;
EXECUTE brin_inclusion_lookup('[25,26)'::int4range);
DEALLOCATE brin_inclusion_lookup;
UPDATE brin_inclusion_rows SET payload = 'matched'
 WHERE span @> 9 RETURNING id, payload;
DELETE FROM brin_inclusion_rows
 WHERE network <<= '192.0.2.0/24'::inet RETURNING id;
SELECT id, payload FROM brin_inclusion_rows ORDER BY id;

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
DROP TABLE brin_inclusion_rows;
DROP TABLE brin_index_clone;
DROP TABLE brin_index_rows;
