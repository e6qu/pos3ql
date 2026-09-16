-- Exercise schema-object widths beyond the former compact storage ceilings.
-- PostgreSQL is the oracle for DDL, catalog projection, and enforcement.
CREATE TYPE differential_wide_composite AS (
  f0 integer, f1 integer, f2 integer, f3 integer, f4 integer,
  f5 integer, f6 integer, f7 integer, f8 integer, f9 integer,
  f10 integer, f11 integer, f12 integer, f13 integer, f14 integer,
  f15 integer, f16 integer
);

CREATE DOMAIN differential_wide_domain AS integer
  CONSTRAINT differential_domain_0 CHECK (VALUE >= 0)
  CONSTRAINT differential_domain_1 CHECK (VALUE >= 1)
  CONSTRAINT differential_domain_2 CHECK (VALUE >= 2)
  CONSTRAINT differential_domain_3 CHECK (VALUE >= 3)
  CONSTRAINT differential_domain_4 CHECK (VALUE >= 4)
  CONSTRAINT differential_domain_5 CHECK (VALUE >= 5)
  CONSTRAINT differential_domain_6 CHECK (VALUE >= 6)
  CONSTRAINT differential_domain_7 CHECK (VALUE >= 7)
  CONSTRAINT differential_domain_8 CHECK (VALUE >= 8);

CREATE TABLE differential_wide_partitioned (
  k0 integer, k1 integer, k2 integer, k3 integer, k4 integer,
  k5 integer, k6 integer, k7 integer, k8 integer
) PARTITION BY RANGE (k0, k1, k2, k3, k4, k5, k6, k7, k8);
CREATE TABLE differential_wide_partition
  PARTITION OF differential_wide_partitioned
  FOR VALUES FROM (0, 0, 0, 0, 0, 0, 0, 0, 0)
             TO (10, 10, 10, 10, 10, 10, 10, 10, 10);
INSERT INTO differential_wide_partitioned VALUES (1, 1, 1, 1, 1, 1, 1, 1, 1);

CREATE TABLE differential_wide_list_parent (value integer)
  PARTITION BY LIST (value);
CREATE TABLE differential_wide_list_child
  PARTITION OF differential_wide_list_parent
  FOR VALUES IN (0, 1, 2, 3, 4, 5, 6, 7, 8);
INSERT INTO differential_wide_list_parent VALUES (8);

CREATE TABLE differential_wide_unique (
  u0 integer, u1 integer, u2 integer, u3 integer, u4 integer,
  u5 integer, u6 integer, u7 integer, u8 integer,
  CONSTRAINT differential_unique_0 UNIQUE (u0),
  CONSTRAINT differential_unique_1 UNIQUE (u1),
  CONSTRAINT differential_unique_2 UNIQUE (u2),
  CONSTRAINT differential_unique_3 UNIQUE (u3),
  CONSTRAINT differential_unique_4 UNIQUE (u4),
  CONSTRAINT differential_unique_5 UNIQUE (u5),
  CONSTRAINT differential_unique_6 UNIQUE (u6),
  CONSTRAINT differential_unique_7 UNIQUE (u7),
  CONSTRAINT differential_unique_8 UNIQUE (u8)
);
INSERT INTO differential_wide_unique VALUES (0, 1, 2, 3, 4, 5, 6, 7, 8);

CREATE TABLE differential_wide_foreign (
  f0 integer, f1 integer, f2 integer, f3 integer, f4 integer,
  f5 integer, f6 integer, f7 integer, f8 integer,
  CONSTRAINT differential_foreign_0 FOREIGN KEY (f0) REFERENCES differential_wide_unique (u0),
  CONSTRAINT differential_foreign_1 FOREIGN KEY (f1) REFERENCES differential_wide_unique (u1),
  CONSTRAINT differential_foreign_2 FOREIGN KEY (f2) REFERENCES differential_wide_unique (u2),
  CONSTRAINT differential_foreign_3 FOREIGN KEY (f3) REFERENCES differential_wide_unique (u3),
  CONSTRAINT differential_foreign_4 FOREIGN KEY (f4) REFERENCES differential_wide_unique (u4),
  CONSTRAINT differential_foreign_5 FOREIGN KEY (f5) REFERENCES differential_wide_unique (u5),
  CONSTRAINT differential_foreign_6 FOREIGN KEY (f6) REFERENCES differential_wide_unique (u6),
  CONSTRAINT differential_foreign_7 FOREIGN KEY (f7) REFERENCES differential_wide_unique (u7),
  CONSTRAINT differential_foreign_8 FOREIGN KEY (f8) REFERENCES differential_wide_unique (u8)
);
INSERT INTO differential_wide_foreign VALUES (0, 1, 2, 3, 4, 5, 6, 7, 8);

CREATE TABLE differential_wide_checks (
  value integer,
  CONSTRAINT differential_check_0 CHECK (value >= 0),
  CONSTRAINT differential_check_1 CHECK (value >= 1),
  CONSTRAINT differential_check_2 CHECK (value >= 2),
  CONSTRAINT differential_check_3 CHECK (value >= 3),
  CONSTRAINT differential_check_4 CHECK (value >= 4),
  CONSTRAINT differential_check_5 CHECK (value >= 5),
  CONSTRAINT differential_check_6 CHECK (value >= 6),
  CONSTRAINT differential_check_7 CHECK (value >= 7),
  CONSTRAINT differential_check_8 CHECK (value >= 8)
);
INSERT INTO differential_wide_checks VALUES (8);

CREATE INDEX differential_wide_index ON differential_wide_unique
  (u0, u1, u2, u3, u4, u5, u6, u7) INCLUDE (u8);

SELECT count(*) FROM pg_attribute
 WHERE attrelid = (SELECT typrelid FROM pg_type
                    WHERE typname = 'differential_wide_composite')
   AND attnum > 0 AND NOT attisdropped;
SELECT count(*) FROM pg_constraint
 WHERE contypid = 'differential_wide_domain'::regtype AND contype = 'c';
SELECT count(*) FROM pg_constraint
 WHERE conrelid = 'differential_wide_unique'::regclass AND contype = 'u';
SELECT count(*) FROM pg_constraint
 WHERE conrelid = 'differential_wide_foreign'::regclass AND contype = 'f';
SELECT count(*) FROM pg_constraint
 WHERE conrelid = 'differential_wide_checks'::regclass AND contype = 'c';
SELECT partnatts, partattrs::text, partdefid = 0
  FROM pg_partitioned_table
 WHERE partrelid = 'differential_wide_partitioned'::regclass;
SELECT indnkeyatts, indnatts
  FROM pg_index
 WHERE indexrelid = 'differential_wide_index'::regclass;
SELECT (SELECT count(*) FROM differential_wide_partitioned),
       (SELECT count(*) FROM differential_wide_list_parent),
       (SELECT count(*) FROM differential_wide_unique),
       (SELECT count(*) FROM differential_wide_foreign),
       (SELECT count(*) FROM differential_wide_checks);

CREATE TABLE differential_empty();
INSERT INTO differential_empty DEFAULT VALUES;
SELECT count(*) FROM differential_empty;
ALTER TABLE differential_empty ADD COLUMN value integer DEFAULT 7;
SELECT value FROM differential_empty;

CREATE TABLE differential_diamond_root (id integer);
CREATE TABLE differential_diamond_left (extra text) INHERITS (differential_diamond_root);
CREATE TABLE differential_diamond_right (extra text) INHERITS (differential_diamond_root);
CREATE TABLE differential_diamond_leaf () INHERITS (differential_diamond_left);
ALTER TABLE differential_diamond_leaf INHERIT differential_diamond_right;
INSERT INTO differential_diamond_leaf VALUES (1, 'leaf');
ALTER TABLE differential_diamond_root ADD COLUMN amount integer DEFAULT 7 NOT NULL;
ALTER TABLE differential_diamond_root RENAME COLUMN amount TO total;
SELECT id, extra, total FROM ONLY differential_diamond_leaf;

CREATE TABLE differential_schema_source (value integer);
CREATE VIEW differential_schema_attributes AS
 SELECT attname, atttypid FROM pg_attribute
 WHERE attrelid = 'differential_schema_source'::regclass AND attnum > 0;
CREATE VIEW differential_schema_classes AS SELECT oid, relname FROM pg_class;
CREATE VIEW differential_schema_columns AS
 SELECT column_name FROM information_schema.columns
 WHERE table_name = 'differential_schema_source';
SELECT attname, atttypid FROM differential_schema_attributes;
SELECT column_name FROM differential_schema_columns;
SELECT attname FROM pg_attribute
 WHERE attrelid = 'differential_schema_attributes'::regclass AND attnum > 0
 ORDER BY attnum;
SELECT column_name FROM information_schema.columns
 WHERE table_name = 'differential_schema_attributes' ORDER BY ordinal_position;
SELECT relname FROM differential_schema_classes
 WHERE oid = 'differential_schema_source'::regclass;

CREATE TABLE differential_hash_source (id int, payload text, active bool);
BEGIN;
CREATE SEQUENCE differential_transaction_sequence START WITH 7;
SELECT (pg_get_sequence_data(oid)).*
  FROM pg_class WHERE relname = 'differential_transaction_sequence';
ALTER SEQUENCE differential_transaction_sequence RESTART WITH 11;
SELECT * FROM pg_get_sequence_data('differential_transaction_sequence'::regclass::oid);
ROLLBACK;
SELECT to_regclass('differential_transaction_sequence') IS NULL;
SELECT pg_typeof(k.oid), pg_typeof(k.conname), pg_typeof(k.conrelid),
       pg_typeof(k.contype), pg_typeof(k.conkey), pg_typeof(k.conpfeqop),
       pg_typeof(k.coninhcount), pg_typeof(k.conbin), pg_typeof(k.tableoid)
  FROM pg_constraint k
 WHERE k.conrelid = 'differential_wide_unique'::regclass AND k.contype = 'u'
 LIMIT 1;
SELECT pg_typeof(d.oid), pg_typeof(d.adrelid), pg_typeof(d.adnum),
       pg_typeof(d.adbin), pg_typeof(d.tableoid)
  FROM pg_attrdef d
 WHERE d.adrelid = 'differential_empty'::regclass;
SELECT c.relname, b.label FROM pg_class c
  LEFT JOIN (VALUES (1259::oid,'class'),(NULL::oid,'never')) b(id,label)
    ON c.tableoid = b.id WHERE c.relname = 'differential_empty';
SELECT b.id, c.relname FROM (VALUES (1259::oid),(NULL::oid)) b(id)
  LEFT JOIN pg_class c ON b.id = c.tableoid AND c.relname = 'differential_empty'
 ORDER BY b.id;
INSERT INTO differential_hash_source VALUES
  (1,'a',true),(1,'b',true),(2,'c',false),(NULL,'n',true),(4,'reject',true);
SELECT p.id, b.payload, (b.details).f1
  FROM (VALUES (1),(1),(2),(3),(NULL),(4)) AS p(id)
  LEFT JOIN (SELECT id, payload, active, ROW(payload, ROW(id)) AS details
               FROM differential_hash_source) AS b
    ON p.id = b.id AND b.active
 WHERE b.payload IS NULL OR b.payload <> 'reject'
 ORDER BY p.id, b.payload;
SELECT p.id, b.payload
  FROM (VALUES (1),(NULL)) AS p(id)
  LEFT JOIN (SELECT id, payload FROM differential_hash_source WHERE FALSE) AS b
    ON p.id = b.id
 ORDER BY p.id;
BEGIN;
ALTER TABLE differential_diamond_root ADD COLUMN temporary integer DEFAULT 9;
ROLLBACK;
SELECT id, extra, total FROM ONLY differential_diamond_leaf;
