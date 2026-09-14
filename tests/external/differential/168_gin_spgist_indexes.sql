-- PostgreSQL 18 GIN and SP-GiST built-in catalogs, DDL, predicates, and cloning.
\pset null 'NULL'

CREATE TABLE gin_spgist_rows (
    id integer,
    tags integer[],
    document tsvector,
    payload jsonb,
    network inet,
    span int4range,
    label text,
    location point,
    covered text
);
CREATE INDEX gin_spgist_tags ON gin_spgist_rows USING gin (tags)
    WITH (fastupdate=off, gin_pending_list_limit=128);
CREATE INDEX gin_spgist_document ON gin_spgist_rows USING gin (document);
CREATE INDEX gin_spgist_payload ON gin_spgist_rows USING gin (payload jsonb_path_ops);
CREATE INDEX gin_spgist_network ON gin_spgist_rows USING spgist (network)
    INCLUDE (covered) WITH (fillfactor=75);
CREATE INDEX gin_spgist_span ON gin_spgist_rows USING spgist (span);
CREATE INDEX gin_spgist_label ON gin_spgist_rows USING spgist (label);
CREATE INDEX gin_spgist_location ON gin_spgist_rows USING spgist (location kd_point_ops);

INSERT INTO gin_spgist_rows VALUES
    (1, ARRAY[1,2], to_tsvector('english', 'quick brown fox'),
     '{"kind":"book","rank":1}'::jsonb, '10.0.0.0/8'::inet,
     '[1,5)'::int4range, 'alpha', '(1,1)'::point, 'one'),
    (2, ARRAY[2,3], to_tsvector('english', 'slow green turtle'),
     '{"kind":"film","rank":2}'::jsonb, '10.1.0.0/16'::inet,
     '[8,14)'::int4range, 'beta', '(2,2)'::point, 'two'),
    (3, ARRAY[8,9], to_tsvector('english', 'quick database'),
     '{"kind":"book","rank":3}'::jsonb, '192.0.2.1'::inet,
     '[20,30)'::int4range, 'omega', '(20,20)'::point, 'three');

SELECT id FROM gin_spgist_rows WHERE tags @> ARRAY[1] ORDER BY id;
SELECT id FROM gin_spgist_rows WHERE tags && ARRAY[3,8] ORDER BY id;
SELECT id FROM gin_spgist_rows WHERE document @@ 'quick'::tsquery ORDER BY id;
SELECT id FROM gin_spgist_rows
 WHERE payload @> '{"kind":"book"}'::jsonb ORDER BY id;
SELECT id FROM gin_spgist_rows
 WHERE network <<= '10.0.0.0/8'::inet ORDER BY id;
SELECT id FROM gin_spgist_rows WHERE span && '[4,10)'::int4range ORDER BY id;
SELECT id FROM gin_spgist_rows WHERE label = 'beta' ORDER BY id;
SELECT id FROM gin_spgist_rows WHERE label ^@ 'be' ORDER BY id;
SELECT id FROM gin_spgist_rows
 WHERE label ~>=~ 'beta' AND label ~<~ 'omega' ORDER BY id;
SELECT id FROM gin_spgist_rows
 WHERE location <@ '((0,0),(3,3))'::box ORDER BY id;

PREPARE gin_lookup(integer[]) AS
    SELECT id FROM gin_spgist_rows WHERE tags @> $1 ORDER BY id;
EXECUTE gin_lookup(ARRAY[2]);
DEALLOCATE gin_lookup;
PREPARE spgist_lookup(int4range) AS
    SELECT id FROM gin_spgist_rows WHERE span && $1 ORDER BY id;
EXECUTE spgist_lookup('[25,26)'::int4range);
DEALLOCATE spgist_lookup;

UPDATE gin_spgist_rows SET covered = 'matched' WHERE tags && ARRAY[3]
 RETURNING id, covered;
DELETE FROM gin_spgist_rows WHERE location ~= '(20,20)'::point RETURNING id;

ALTER INDEX gin_spgist_tags SET (fastupdate=on, gin_pending_list_limit=256);
ALTER INDEX gin_spgist_tags RESET (fastupdate);
ALTER INDEX gin_spgist_network SET (fillfactor=70);
SELECT indexname, indexdef FROM pg_indexes
 WHERE tablename = 'gin_spgist_rows' ORDER BY indexname;
SELECT index_relation.relname, access_method.amname,
       index_catalog.indclass::text, index_relation.reloptions::text
  FROM pg_index AS index_catalog
  JOIN pg_class AS index_relation ON index_relation.oid = index_catalog.indexrelid
  JOIN pg_am AS access_method ON access_method.oid = index_relation.relam
 WHERE index_relation.relname LIKE 'gin_spgist_%'
 ORDER BY index_relation.relname;

SELECT oid, opcname, opcfamily, opcintype, opcdefault, opckeytype
  FROM pg_opclass WHERE opcmethod IN (2742,4000) ORDER BY oid;
SELECT oid, opfmethod, opfname, opfnamespace, opfowner
  FROM pg_opfamily WHERE opfmethod IN (2742,4000) ORDER BY oid;
SELECT oid, amopfamily, amoplefttype, amoprighttype, amopstrategy,
       amoppurpose, amopopr, amopmethod, amopsortfamily
  FROM pg_amop WHERE amopmethod IN (2742,4000) ORDER BY oid;
SELECT procedure.oid, procedure.amprocfamily, procedure.amproclefttype,
       procedure.amprocrighttype, procedure.amprocnum, procedure.amproc
  FROM pg_amproc AS procedure
  JOIN pg_opfamily AS family ON family.oid = procedure.amprocfamily
 WHERE family.opfmethod IN (2742,4000)
 ORDER BY procedure.oid;

CREATE TABLE gin_spgist_clone (LIKE gin_spgist_rows INCLUDING INDEXES);
SELECT access_method.amname, index_catalog.indclass::text,
       index_relation.reloptions::text
  FROM pg_index AS index_catalog
  JOIN pg_class AS index_relation ON index_relation.oid = index_catalog.indexrelid
  JOIN pg_am AS access_method ON access_method.oid = index_relation.relam
 WHERE index_catalog.indrelid = 'gin_spgist_clone'::regclass
 ORDER BY index_relation.relname;

CREATE TABLE gin_spgist_partitioned (id integer, tags integer[])
    PARTITION BY RANGE (id);
CREATE TABLE gin_spgist_partition_low PARTITION OF gin_spgist_partitioned
    FOR VALUES FROM (0) TO (10);
CREATE TABLE gin_spgist_partition_high PARTITION OF gin_spgist_partitioned
    FOR VALUES FROM (10) TO (20);
CREATE INDEX gin_spgist_partitioned_tags
    ON gin_spgist_partitioned USING gin (tags);
INSERT INTO gin_spgist_partitioned VALUES (4, ARRAY[4]), (14, ARRAY[14]);
SELECT id FROM gin_spgist_partitioned WHERE tags @> ARRAY[14] ORDER BY id;
SELECT parent.relname, parent_method.amname, child.relname, child_method.amname
  FROM pg_inherits AS inheritance
  JOIN pg_class AS parent ON parent.oid = inheritance.inhparent
  JOIN pg_am AS parent_method ON parent_method.oid = parent.relam
  JOIN pg_class AS child ON child.oid = inheritance.inhrelid
  JOIN pg_am AS child_method ON child_method.oid = child.relam
 WHERE parent.relname = 'gin_spgist_partitioned_tags'
 ORDER BY child.relname;

REINDEX TABLE gin_spgist_rows;
SELECT id, covered FROM gin_spgist_rows ORDER BY id;

DROP TABLE gin_spgist_partitioned CASCADE;
DROP TABLE gin_spgist_clone;
DROP TABLE gin_spgist_rows;
