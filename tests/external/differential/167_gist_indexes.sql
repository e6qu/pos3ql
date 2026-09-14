-- PostgreSQL 18 GiST built-in classes, catalogs, DDL, predicates, and cloning.
\pset null 'NULL'

CREATE TABLE gist_index_rows (
    id integer,
    span int4range,
    spans int4multirange,
    network inet,
    location point,
    area box,
    shape polygon,
    radius circle,
    document tsvector,
    query tsquery,
    payload text
);
CREATE INDEX gist_index_span ON gist_index_rows USING gist (span)
    INCLUDE (payload) WITH (fillfactor=80, buffering=on);
CREATE INDEX gist_index_spans ON gist_index_rows USING gist (spans);
CREATE INDEX gist_index_network ON gist_index_rows USING gist (network inet_ops);
CREATE INDEX gist_index_location ON gist_index_rows USING gist (location);
CREATE INDEX gist_index_area ON gist_index_rows USING gist (area);
CREATE INDEX gist_index_shape ON gist_index_rows USING gist (shape);
CREATE INDEX gist_index_radius ON gist_index_rows USING gist (radius);
CREATE INDEX gist_index_document ON gist_index_rows USING gist
    (document tsvector_ops(siglen=64));
CREATE INDEX gist_index_query ON gist_index_rows USING gist (query);

INSERT INTO gist_index_rows VALUES
    (1, '[1,5)'::int4range, '{[1,5)}'::int4multirange,
     '10.0.0.0/8'::inet, '(1,1)'::point, '((0,0),(3,3))'::box,
     '((0,0),(0,3),(3,0))'::polygon, '<(1,1),2>'::circle,
     to_tsvector('english', 'quick brown fox'), 'quick'::tsquery, 'one'),
    (2, '[8,14)'::int4range, '{[8,14)}'::int4multirange,
     '10.1.0.0/16'::inet, '(2,2)'::point, '((1,1),(4,4))'::box,
     '((1,1),(1,4),(4,1))'::polygon, '<(2,2),1>'::circle,
     to_tsvector('english', 'slow green turtle'), 'slow'::tsquery, 'two'),
    (3, '[20,30)'::int4range, '{[20,30)}'::int4multirange,
     '192.0.2.1'::inet, '(20,20)'::point, '((19,19),(21,21))'::box,
     '((19,19),(19,21),(21,19))'::polygon, '<(20,20),1>'::circle,
     to_tsvector('english', 'quick database'), 'database'::tsquery, 'three');
ANALYZE gist_index_rows;

SELECT id FROM gist_index_rows WHERE span && '[4,10)'::int4range ORDER BY id;
SELECT id FROM gist_index_rows
 WHERE spans && '{[13,21)}'::int4multirange ORDER BY id;
SELECT id FROM gist_index_rows
 WHERE network <<= '10.0.0.0/8'::inet ORDER BY id;
SELECT id FROM gist_index_rows
 WHERE location <@ '((0,0),(3,3))'::box ORDER BY id;
SELECT id FROM gist_index_rows WHERE location <<| '(3,3)'::point ORDER BY id;
SELECT id FROM gist_index_rows WHERE location ~= '(2,2)'::point ORDER BY id;
SELECT id FROM gist_index_rows
 WHERE area && '((2.5,2.5),(3.5,3.5))'::box ORDER BY id;
SELECT id FROM gist_index_rows
 WHERE area |>> '((0,0),(3,3))'::box ORDER BY id;
SELECT id FROM gist_index_rows
 WHERE shape && '((2,2),(2,5),(5,2))'::polygon ORDER BY id;
SELECT id FROM gist_index_rows
 WHERE radius && '<(2,2),1>'::circle ORDER BY id;
SELECT id FROM gist_index_rows WHERE document @@ 'quick'::tsquery ORDER BY id;
SELECT id FROM gist_index_rows WHERE query @> 'quick'::tsquery ORDER BY id;
SELECT id FROM gist_index_rows WHERE network <> '10.1.0.0/16'::inet ORDER BY id;
SELECT id FROM gist_index_rows WHERE span = '[8,14)'::int4range ORDER BY id;

PREPARE gist_span_lookup(int4range) AS
    SELECT id FROM gist_index_rows WHERE span && $1 ORDER BY id;
EXECUTE gist_span_lookup('[25,26)'::int4range);
DEALLOCATE gist_span_lookup;
UPDATE gist_index_rows SET payload = 'matched' WHERE span @> 9
 RETURNING id, payload;
DELETE FROM gist_index_rows WHERE location ~= '(20,20)'::point RETURNING id;

ALTER INDEX gist_index_span SET (fillfactor=75, buffering=auto);
ALTER INDEX gist_index_span RESET (buffering);
SELECT indexname, indexdef FROM pg_indexes
 WHERE tablename = 'gist_index_rows' ORDER BY indexname;
SELECT index_relation.relname, access_method.amname,
       index_catalog.indclass::text, index_relation.reloptions::text
  FROM pg_index AS index_catalog
  JOIN pg_class AS index_relation ON index_relation.oid = index_catalog.indexrelid
  JOIN pg_am AS access_method ON access_method.oid = index_relation.relam
 WHERE index_relation.relname LIKE 'gist_index_%'
 ORDER BY index_relation.relname;
SELECT relation.relname, attribute.attname, attribute.attoptions::text
  FROM pg_attribute AS attribute
  JOIN pg_class AS relation ON relation.oid = attribute.attrelid
 WHERE relation.relname = 'gist_index_document' AND attribute.attnum > 0;

SELECT oid, opcname, opcfamily, opcintype, opcdefault, opckeytype
  FROM pg_opclass WHERE opcmethod = 783 ORDER BY oid;
SELECT oid, opfname, opfnamespace, opfowner
  FROM pg_opfamily WHERE opfmethod = 783 ORDER BY oid;
SELECT oid, amopfamily, amoplefttype, amoprighttype, amopstrategy,
       amoppurpose, amopopr, amopmethod, amopsortfamily
  FROM pg_amop WHERE amopmethod = 783 ORDER BY oid;
SELECT procedure.oid, procedure.amprocfamily, procedure.amproclefttype,
       procedure.amprocrighttype, procedure.amprocnum, procedure.amproc
  FROM pg_amproc AS procedure
  JOIN pg_opfamily AS family ON family.oid = procedure.amprocfamily
 WHERE family.opfmethod = 783
 ORDER BY procedure.oid;

CREATE TABLE gist_index_clone (LIKE gist_index_rows INCLUDING INDEXES);
SELECT access_method.amname, index_catalog.indclass::text,
       index_relation.reloptions::text
  FROM pg_index AS index_catalog
  JOIN pg_class AS index_relation ON index_relation.oid = index_catalog.indexrelid
  JOIN pg_am AS access_method ON access_method.oid = index_relation.relam
 WHERE index_catalog.indrelid = 'gist_index_clone'::regclass
 ORDER BY index_relation.relname;

CREATE TABLE gist_index_partitioned (id integer, span int4range)
    PARTITION BY RANGE (id);
CREATE TABLE gist_index_partition_low PARTITION OF gist_index_partitioned
    FOR VALUES FROM (0) TO (10);
CREATE TABLE gist_index_partition_high PARTITION OF gist_index_partitioned
    FOR VALUES FROM (10) TO (20);
CREATE INDEX gist_index_partitioned_span
    ON gist_index_partitioned USING gist (span);
INSERT INTO gist_index_partitioned VALUES
    (4, '[4,5)'::int4range), (14, '[14,15)'::int4range);
SELECT id FROM gist_index_partitioned
 WHERE span && '[14,16)'::int4range ORDER BY id;
SELECT parent.relname, parent_method.amname, child.relname, child_method.amname
  FROM pg_inherits AS inheritance
  JOIN pg_class AS parent ON parent.oid = inheritance.inhparent
  JOIN pg_am AS parent_method ON parent_method.oid = parent.relam
  JOIN pg_class AS child ON child.oid = inheritance.inhrelid
  JOIN pg_am AS child_method ON child_method.oid = child.relam
 WHERE parent.relname = 'gist_index_partitioned_span'
 ORDER BY child.relname;

REINDEX TABLE gist_index_rows;
SELECT id, payload FROM gist_index_rows ORDER BY id;

DROP TABLE gist_index_partitioned CASCADE;
DROP TABLE gist_index_clone;
DROP TABLE gist_index_rows;
