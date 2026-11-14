-- ALTER TABLE carries column changes from a partitioned table to every
-- partition at every level, as PostgreSQL's ALTER recursion does.
CREATE TABLE pr455 (a int, b int, note text) PARTITION BY RANGE (a);
CREATE TABLE pr455_1 PARTITION OF pr455 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (b);
CREATE TABLE pr455_11 PARTITION OF pr455_1 FOR VALUES IN (1, 2);
CREATE TABLE pr455_12 PARTITION OF pr455_1 DEFAULT;
CREATE TABLE pr455_2 PARTITION OF pr455 FOR VALUES FROM (10) TO (20);
CREATE INDEX pr455_note ON pr455 (note);
INSERT INTO pr455 VALUES (1, 1, 'one'), (2, 5, 'two'), (12, 3, 'twelve');
ALTER TABLE pr455 ADD COLUMN c text DEFAULT 'x', ADD CONSTRAINT pr455_c CHECK (c <> '');
SELECT tableoid::regclass, * FROM pr455 ORDER BY a;
ALTER TABLE pr455 RENAME COLUMN note TO label;
SELECT indexrelid::regclass, pg_get_indexdef(indexrelid) FROM pg_index
 WHERE indrelid::regclass::text LIKE 'pr455%' ORDER BY 1;
ALTER TABLE pr455 ALTER COLUMN label TYPE varchar(20), ALTER COLUMN c SET DEFAULT 'y';
ALTER TABLE pr455 ALTER COLUMN c SET STATISTICS 50, ALTER COLUMN label SET STORAGE MAIN;
INSERT INTO pr455 (a, b, label) VALUES (3, 2, 'three');
SELECT c.relname, a.attname, format_type(a.atttypid, a.atttypmod), a.attstattarget,
       a.attstorage, a.attislocal, a.attinhcount, pg_get_expr(d.adbin, d.adrelid)
  FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid
  LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
 WHERE c.relname LIKE 'pr455%' AND c.relkind IN ('r', 'p') AND a.attnum > 0
   AND NOT a.attisdropped
 ORDER BY c.relname, a.attname;
ALTER TABLE pr455 DROP COLUMN c;
SELECT tableoid::regclass, * FROM pr455 ORDER BY a;
-- Partition key columns and partitions' own copies are protected.
ALTER TABLE pr455 ALTER COLUMN a TYPE bigint;
ALTER TABLE pr455 DROP COLUMN b;
ALTER TABLE pr455_1 DROP COLUMN label;
ALTER TABLE pr455_11 RENAME COLUMN label TO l;
ALTER TABLE pr455_2 ALTER COLUMN label TYPE text;
ALTER TABLE pr455_2 ADD COLUMN z int;
-- ONLY cannot split a partitioned table from its partitions.
ALTER TABLE ONLY pr455 ADD COLUMN d int;
ALTER TABLE ONLY pr455 DROP COLUMN label;
ALTER TABLE ONLY pr455 RENAME COLUMN label TO l;
ALTER TABLE ONLY pr455 ALTER COLUMN label TYPE text;
ALTER TABLE ONLY pr455 ALTER COLUMN label SET DEFAULT 'only';
SELECT c.relname, pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d
  JOIN pg_class c ON c.oid = d.adrelid WHERE c.relname LIKE 'pr455%' ORDER BY 1;
-- A partitioned table without partitions accepts ONLY.
CREATE TABLE pr455_empty (a int) PARTITION BY LIST (a);
ALTER TABLE ONLY pr455_empty ADD COLUMN b int;
-- Ordinary inheritance children follow the same recursion.
CREATE TABLE ih455 (a int, b int);
CREATE TABLE ih455_c () INHERITS (ih455);
ALTER TABLE ih455 ALTER COLUMN b SET STATISTICS 7, ALTER COLUMN b SET STORAGE PLAIN;
SELECT attrelid::regclass, attstattarget, attstorage FROM pg_attribute
 WHERE attrelid IN ('ih455'::regclass, 'ih455_c'::regclass) AND attname = 'b' ORDER BY 1;
ALTER TABLE ih455_c RENAME COLUMN b TO bb;
ALTER TABLE ih455_c ALTER COLUMN b TYPE bigint;
ALTER TABLE ONLY ih455 DROP COLUMN b;
SELECT attrelid::regclass, attname, attislocal, attinhcount FROM pg_attribute
 WHERE attrelid IN ('ih455'::regclass, 'ih455_c'::regclass) AND attnum > 0
   AND NOT attisdropped ORDER BY 1, attnum;
-- A recursing drop keeps a column the child also declares, or that another
-- parent still supplies.
CREATE TABLE ih455_p1 (a int, b int);
CREATE TABLE ih455_p2 (b int);
CREATE TABLE ih455_d (a int) INHERITS (ih455_p1, ih455_p2);
ALTER TABLE ih455_p1 DROP COLUMN a, DROP COLUMN b;
SELECT attname, attislocal, attinhcount FROM pg_attribute
 WHERE attrelid = 'ih455_d'::regclass AND attnum > 0 AND NOT attisdropped ORDER BY attnum;
ALTER TABLE ih455_d NO INHERIT ih455_p2;
SELECT attname, attislocal, attinhcount FROM pg_attribute
 WHERE attrelid = 'ih455_d'::regclass AND attnum > 0 AND NOT attisdropped ORDER BY attnum;
-- Attaching makes a table's columns inherited; detaching makes them local.
CREATE TABLE pr455_3 (a int, b int, label varchar(20));
ALTER TABLE pr455 ATTACH PARTITION pr455_3 FOR VALUES FROM (20) TO (30);
SELECT attname, attislocal, attinhcount FROM pg_attribute
 WHERE attrelid = 'pr455_3'::regclass AND attnum > 0 AND NOT attisdropped ORDER BY attnum;
ALTER TABLE pr455 DETACH PARTITION pr455_3;
SELECT attname, attislocal, attinhcount FROM pg_attribute
 WHERE attrelid = 'pr455_3'::regclass AND attnum > 0 AND NOT attisdropped ORDER BY attnum;
-- Storage and compression need a TOAST-capable type.
CREATE TABLE st455 (a int STORAGE EXTERNAL);
CREATE TABLE st455 (a int COMPRESSION pglz);
