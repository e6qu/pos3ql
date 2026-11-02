CREATE TABLE inherited_identity_source(id integer GENERATED ALWAYS AS IDENTITY
  (START WITH 11 INCREMENT BY 3 CACHE 2 CYCLE MAXVALUE 100));
INSERT INTO inherited_identity_source DEFAULT VALUES RETURNING id;
CREATE TABLE inherited_identity_copy(LIKE inherited_identity_source INCLUDING IDENTITY);
CREATE TABLE inherited_identity_plain(LIKE inherited_identity_source INCLUDING DEFAULTS);
INSERT INTO inherited_identity_copy DEFAULT VALUES RETURNING id;
INSERT INTO inherited_identity_source DEFAULT VALUES RETURNING id;
INSERT INTO inherited_identity_copy DEFAULT VALUES RETURNING id;
SELECT seqstart,seqincrement,seqcache,seqcycle,seqmax FROM pg_sequence
  JOIN pg_class ON pg_class.oid=seqrelid WHERE relname IN
  ('inherited_identity_source_id_seq','inherited_identity_copy_id_seq') ORDER BY relname;
SELECT pg_get_serial_sequence('inherited_identity_source','id'),
  pg_get_serial_sequence('inherited_identity_copy','id'),
  pg_get_serial_sequence('inherited_identity_plain','id');
INSERT INTO inherited_identity_plain DEFAULT VALUES;
CREATE TABLE inherited_identity_child() INHERITS(inherited_identity_source);
INSERT INTO inherited_identity_child DEFAULT VALUES;
INSERT INTO inherited_identity_child(id) VALUES(101) RETURNING id;

CREATE TABLE inherited_serial_source(id serial);
CREATE TABLE inherited_serial_defaults(LIKE inherited_serial_source INCLUDING DEFAULTS);
CREATE TABLE inherited_serial_all(LIKE inherited_serial_source INCLUDING ALL);
CREATE TABLE inherited_serial_identity(LIKE inherited_serial_source INCLUDING IDENTITY);
CREATE TABLE inherited_serial_child() INHERITS(inherited_serial_source);
INSERT INTO inherited_serial_source DEFAULT VALUES RETURNING id;
INSERT INTO inherited_serial_defaults DEFAULT VALUES RETURNING id;
INSERT INTO inherited_serial_all DEFAULT VALUES RETURNING id;
INSERT INTO inherited_serial_child DEFAULT VALUES RETURNING id;
INSERT INTO inherited_serial_identity DEFAULT VALUES;
SELECT pg_get_serial_sequence('inherited_serial_source','id'),
  pg_get_serial_sequence('inherited_serial_defaults','id'),
  pg_get_serial_sequence('inherited_serial_all','id'),
  pg_get_serial_sequence('inherited_serial_identity','id'),
  pg_get_serial_sequence('inherited_serial_child','id');
SELECT relation.relname,sequence.relname,dependency.deptype FROM pg_depend dependency
  JOIN pg_class sequence ON sequence.oid=dependency.objid AND sequence.relkind='S'
  JOIN pg_class relation ON relation.oid=dependency.refobjid
  WHERE dependency.classid='pg_class'::regclass AND dependency.refclassid='pg_class'::regclass
    AND dependency.refobjsubid=1 AND relation.relname IN
    ('inherited_identity_source','inherited_identity_copy','inherited_identity_child',
     'inherited_serial_source','inherited_serial_defaults','inherited_serial_all',
     'inherited_serial_identity','inherited_serial_child') ORDER BY relation.relname;
DROP TABLE inherited_identity_child,inherited_identity_plain,inherited_identity_copy;
DROP TABLE inherited_identity_source;
DROP TABLE inherited_serial_child,inherited_serial_identity,inherited_serial_all,inherited_serial_defaults;
DROP TABLE inherited_serial_source;

CREATE TABLE partition_identity_root(id integer GENERATED ALWAYS AS IDENTITY
  (START WITH 7 INCREMENT BY 3),payload integer) PARTITION BY RANGE(payload);
CREATE TABLE partition_identity_a PARTITION OF partition_identity_root FOR VALUES FROM(0) TO(10);
CREATE TABLE partition_identity_b PARTITION OF partition_identity_root FOR VALUES FROM(10) TO(20);
INSERT INTO partition_identity_root(payload) VALUES(1) RETURNING id;
INSERT INTO partition_identity_a(payload) VALUES(2) RETURNING id;
INSERT INTO partition_identity_b(payload) VALUES(12) RETURNING id;
INSERT INTO partition_identity_root(payload) VALUES(13) RETURNING id;
CREATE TABLE partition_identity_attached(payload integer,id integer NOT NULL);
BEGIN;
ALTER TABLE partition_identity_root ATTACH PARTITION partition_identity_attached FOR VALUES FROM(20) TO(30);
ROLLBACK;
SELECT attidentity::text FROM pg_attribute
  WHERE attrelid='partition_identity_attached'::regclass AND attname='id';
ALTER TABLE partition_identity_root ATTACH PARTITION partition_identity_attached FOR VALUES FROM(20) TO(30);
INSERT INTO partition_identity_attached(payload) VALUES(21) RETURNING id;
INSERT INTO partition_identity_root(payload) VALUES(22) RETURNING id;
CREATE TABLE partition_identity_copy(LIKE partition_identity_a INCLUDING IDENTITY);
INSERT INTO partition_identity_copy DEFAULT VALUES RETURNING id;
TRUNCATE partition_identity_a RESTART IDENTITY;
INSERT INTO partition_identity_root(payload) VALUES(3) RETURNING id;
SELECT pg_get_serial_sequence('partition_identity_root','id'),
  pg_get_serial_sequence('partition_identity_a','id'),
  pg_get_serial_sequence('partition_identity_attached','id'),
  pg_get_serial_sequence('partition_identity_copy','id');
SELECT relation.relname,attribute.attnum,attribute.attidentity::text,attribute.attnotnull
  FROM pg_class relation JOIN pg_attribute attribute ON attribute.attrelid=relation.oid
  WHERE relation.relname IN('partition_identity_root','partition_identity_a','partition_identity_attached')
    AND attribute.attname='id' ORDER BY relation.relname;
DROP TABLE partition_identity_copy;
DROP TABLE partition_identity_root;

CREATE TABLE serial_partition_root(id serial,payload integer) PARTITION BY RANGE(payload);
CREATE TABLE serial_partition_leaf PARTITION OF serial_partition_root FOR VALUES FROM(0) TO(10);
INSERT INTO serial_partition_root(payload) VALUES(1) RETURNING id;
INSERT INTO serial_partition_leaf(payload) VALUES(2) RETURNING id;
SELECT pg_get_serial_sequence('serial_partition_leaf','id');
DROP TABLE serial_partition_root;

CREATE TABLE nested_identity_root(id integer GENERATED ALWAYS AS IDENTITY
  (START WITH 7 INCREMENT BY 3),payload integer) PARTITION BY RANGE(payload);
CREATE TABLE nested_identity_sub PARTITION OF nested_identity_root
  FOR VALUES FROM(0) TO(100) PARTITION BY RANGE(payload);
CREATE TABLE nested_identity_leaf PARTITION OF nested_identity_sub FOR VALUES FROM(0) TO(100);
INSERT INTO nested_identity_root(payload) VALUES(1) RETURNING id;
INSERT INTO nested_identity_sub(payload) VALUES(2) RETURNING id;
INSERT INTO nested_identity_leaf(payload) VALUES(3) RETURNING id;
BEGIN;
ALTER TABLE nested_identity_root DETACH PARTITION nested_identity_sub;
ROLLBACK;
INSERT INTO nested_identity_leaf(payload) VALUES(4) RETURNING id;
ALTER TABLE nested_identity_root DETACH PARTITION nested_identity_sub;
SELECT attidentity::text FROM pg_attribute WHERE attrelid='nested_identity_sub'::regclass AND attname='id';
SELECT attidentity::text FROM pg_attribute WHERE attrelid='nested_identity_leaf'::regclass AND attname='id';
INSERT INTO nested_identity_sub(payload) VALUES(5);
INSERT INTO nested_identity_leaf(payload) VALUES(6);
CREATE TABLE nested_identity_owned(payload integer,id integer GENERATED ALWAYS AS IDENTITY);
ALTER TABLE nested_identity_root ATTACH PARTITION nested_identity_owned FOR VALUES FROM(100) TO(200);
INSERT INTO nested_identity_owned(payload) VALUES(101) RETURNING id;
DROP TABLE nested_identity_owned;
DROP TABLE nested_identity_sub;
DROP TABLE nested_identity_root;

CREATE TABLE mode_identity_root(id bigint GENERATED BY DEFAULT AS IDENTITY,payload integer NOT NULL)
  PARTITION BY RANGE(payload);
CREATE TABLE mode_identity_sub PARTITION OF mode_identity_root
  FOR VALUES FROM(0) TO(100) PARTITION BY RANGE(payload);
CREATE TABLE mode_identity_leaf PARTITION OF mode_identity_sub FOR VALUES FROM(0) TO(100);
CREATE TABLE mode_identity_attached(payload integer NOT NULL,id bigint NOT NULL);
ALTER TABLE mode_identity_root ATTACH PARTITION mode_identity_attached FOR VALUES FROM(100) TO(200);
ALTER TABLE mode_identity_root ALTER COLUMN id SET GENERATED ALWAYS;
SELECT c.relname,a.attidentity::text FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid
  WHERE c.relname IN('mode_identity_root','mode_identity_sub','mode_identity_leaf','mode_identity_attached')
    AND a.attname='id' ORDER BY c.relname;
INSERT INTO mode_identity_leaf(id,payload) VALUES(901,1);
INSERT INTO mode_identity_attached(id,payload) VALUES(902,101);
BEGIN;
ALTER TABLE mode_identity_root ALTER COLUMN id SET GENERATED BY DEFAULT;
SELECT c.relname,a.attidentity::text FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid
  WHERE c.relname IN('mode_identity_root','mode_identity_sub','mode_identity_leaf','mode_identity_attached')
    AND a.attname='id' ORDER BY c.relname;
ROLLBACK;
ALTER TABLE ONLY mode_identity_root ALTER COLUMN id SET GENERATED BY DEFAULT;
ALTER TABLE mode_identity_leaf ALTER COLUMN id SET GENERATED BY DEFAULT;
ALTER TABLE mode_identity_root ALTER COLUMN id SET GENERATED BY DEFAULT,ALTER COLUMN payload SET NOT NULL;
INSERT INTO mode_identity_leaf(id,payload) VALUES(903,2) RETURNING id;
INSERT INTO mode_identity_attached(id,payload) VALUES(904,102) RETURNING id;
SELECT c.relname,a.attidentity::text FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid
  WHERE c.relname IN('mode_identity_root','mode_identity_sub','mode_identity_leaf','mode_identity_attached')
    AND a.attname='id' ORDER BY c.relname;
DROP TABLE mode_identity_root;

CREATE TABLE empty_mode_identity_root(id integer GENERATED BY DEFAULT AS IDENTITY,payload integer)
  PARTITION BY RANGE(payload);
ALTER TABLE ONLY empty_mode_identity_root ALTER COLUMN id SET GENERATED ALWAYS;
CREATE TABLE empty_mode_identity_leaf PARTITION OF empty_mode_identity_root FOR VALUES FROM(0) TO(10);
ALTER TABLE empty_mode_identity_leaf ALTER COLUMN missing SET GENERATED ALWAYS;
ALTER TABLE empty_mode_identity_leaf ALTER COLUMN payload SET GENERATED ALWAYS;
ALTER TABLE empty_mode_identity_leaf ALTER COLUMN id SET GENERATED BY DEFAULT;
ALTER TABLE empty_mode_identity_root ALTER COLUMN payload SET GENERATED ALWAYS;
DROP TABLE empty_mode_identity_root;
