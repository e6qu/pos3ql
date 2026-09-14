-- PostgreSQL 18 hash-index DDL, catalogs, equality execution, and index cloning.
\pset null 'NULL'

CREATE TYPE hash_index_mood AS ENUM ('low', 'medium', 'high');
CREATE TABLE hash_index_rows (
    id integer,
    email text,
    token uuid,
    mood hash_index_mood,
    active boolean,
    document jsonb
);
CREATE INDEX hash_index_rows_id ON hash_index_rows USING hash (id)
    NULLS NOT DISTINCT WITH (fillfactor = 80);
CREATE INDEX hash_index_rows_email ON hash_index_rows USING hash
    (lower(email) text_pattern_ops) WHERE active;
CREATE INDEX hash_index_rows_token ON hash_index_rows USING hash (token);
CREATE INDEX hash_index_rows_mood ON hash_index_rows USING hash (mood);
CREATE INDEX hash_index_rows_document ON hash_index_rows USING hash (document);

INSERT INTO hash_index_rows VALUES
    (1, 'Alpha@example.com', '00000000-0000-0000-0000-000000000001',
     'low', true, '{"kind":"alpha"}'),
    (2, 'Beta@example.com', '00000000-0000-0000-0000-000000000002',
     'medium', false, '{"kind":"beta"}'),
    (3, 'Gamma@example.com', '00000000-0000-0000-0000-000000000003',
     'high', true, '{"kind":"gamma"}'),
    (NULL, NULL, NULL, NULL, true, NULL);

SELECT id, email FROM hash_index_rows WHERE id = 2;
PREPARE hash_index_lookup(integer) AS
    SELECT email FROM hash_index_rows WHERE id = $1;
EXECUTE hash_index_lookup(3);
DEALLOCATE hash_index_lookup;
SELECT id FROM hash_index_rows
 WHERE active AND lower(email) = 'alpha@example.com';
SELECT id FROM hash_index_rows
 WHERE token = '00000000-0000-0000-0000-000000000003';
SELECT id FROM hash_index_rows WHERE mood = 'medium';
SELECT id FROM hash_index_rows WHERE document = '{"kind":"alpha"}';
UPDATE hash_index_rows SET email = 'Beta+updated@example.com' WHERE id = 2
 RETURNING id, email;
DELETE FROM hash_index_rows WHERE id = 1 RETURNING id;
SELECT count(*) FROM hash_index_rows WHERE id IS NULL;

SELECT indexname, indexdef
  FROM pg_indexes
 WHERE tablename = 'hash_index_rows'
 ORDER BY indexname;
SELECT index_relation.relname, access_method.amname,
       index_catalog.indclass::text,
       index_catalog.indisunique,
       index_catalog.indnullsnotdistinct
  FROM pg_index AS index_catalog
  JOIN pg_class AS index_relation
    ON index_relation.oid = index_catalog.indexrelid
  JOIN pg_am AS access_method ON access_method.oid = index_relation.relam
 WHERE index_relation.relname LIKE 'hash_index_rows_%'
 ORDER BY index_relation.relname;

CREATE TABLE hash_index_clone
    (LIKE hash_index_rows INCLUDING INDEXES);
SELECT access_method.amname, count(*)
  FROM pg_index AS index_catalog
  JOIN pg_class AS index_relation
    ON index_relation.oid = index_catalog.indexrelid
  JOIN pg_am AS access_method ON access_method.oid = index_relation.relam
 WHERE index_catalog.indrelid = 'hash_index_clone'::regclass
 GROUP BY access_method.amname;

CREATE TABLE hash_index_partitioned (id integer, payload text)
    PARTITION BY RANGE (id);
CREATE TABLE hash_index_partition_low PARTITION OF hash_index_partitioned
    FOR VALUES FROM (0) TO (10);
CREATE TABLE hash_index_partition_high PARTITION OF hash_index_partitioned
    FOR VALUES FROM (10) TO (20);
CREATE INDEX hash_index_partitioned_id
    ON hash_index_partitioned USING hash (id);
INSERT INTO hash_index_partitioned VALUES (4, 'low'), (14, 'high');
SELECT payload FROM hash_index_partitioned WHERE id = 14;
SELECT parent.relname, parent_method.amname, child.relname, child_method.amname
  FROM pg_inherits AS inheritance
  JOIN pg_class AS parent ON parent.oid = inheritance.inhparent
  JOIN pg_am AS parent_method ON parent_method.oid = parent.relam
  JOIN pg_class AS child ON child.oid = inheritance.inhrelid
  JOIN pg_am AS child_method ON child_method.oid = child.relam
 WHERE parent.relname = 'hash_index_partitioned_id'
 ORDER BY child.relname;

REINDEX TABLE hash_index_rows;
SELECT email FROM hash_index_rows WHERE id = 2;

SELECT (SELECT count(*) FROM pg_opclass WHERE opcmethod = 405),
       (SELECT count(*) FROM pg_opfamily WHERE opfmethod = 405),
       (SELECT count(*) FROM pg_amop WHERE amopmethod = 405),
       (SELECT count(*) FROM pg_amproc AS procedure
          JOIN pg_opfamily AS family ON family.oid = procedure.amprocfamily
         WHERE family.opfmethod = 405);
SELECT oid, opcname, opcfamily, opcintype, opcdefault, opckeytype
  FROM pg_opclass WHERE opcmethod = 405 ORDER BY oid;
SELECT oid, opfname, opfnamespace, opfowner
  FROM pg_opfamily WHERE opfmethod = 405 ORDER BY oid;
SELECT oid, amopfamily, amoplefttype, amoprighttype, amopstrategy,
       amoppurpose, amopopr, amopsortfamily
  FROM pg_amop WHERE amopmethod = 405 ORDER BY oid;
SELECT procedure.oid, procedure.amprocfamily, procedure.amproclefttype,
       procedure.amprocrighttype, procedure.amprocnum, procedure.amproc
  FROM pg_amproc AS procedure
  JOIN pg_opfamily AS family ON family.oid = procedure.amprocfamily
 WHERE family.opfmethod = 405
 ORDER BY procedure.oid;

DROP TABLE hash_index_partitioned CASCADE;
DROP TABLE hash_index_clone;
DROP TABLE hash_index_rows;
DROP TYPE hash_index_mood;
