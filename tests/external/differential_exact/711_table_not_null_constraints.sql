-- PostgreSQL 18's table-level NOT NULL constraint (`[CONSTRAINT name] NOT
-- NULL column [NO INHERIT]`): CREATE TABLE merges it with the columns' own
-- in the order written (AddRelationNotNullConstraints), naming conflicts and
-- NO INHERIT mismatches fail, and an inherited column takes no NO INHERIT
-- one. ALTER TABLE ADD keeps a column's existing constraint
-- (AdjustNotNullInheritance), checks its name and NO INHERIT state, and
-- recurses to inheritance children and partitions.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/heap.c
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/pg_constraint.c
CREATE TABLE t1 (a integer, b integer, CONSTRAINT t1_a_not_null NOT NULL b, NOT NULL a);
CREATE TABLE t2 (a integer CONSTRAINT x NOT NULL, CONSTRAINT y NOT NULL a);
CREATE TABLE t2 (a integer CONSTRAINT x NOT NULL, CONSTRAINT x NOT NULL a, b integer NOT NULL, CONSTRAINT q NOT NULL b);
CREATE TABLE t3 (a integer NOT NULL, NOT NULL a NO INHERIT);
CREATE TABLE t3 (a integer NOT NULL NO INHERIT, NOT NULL a);
CREATE TABLE t3 (a integer NOT NULL NO INHERIT, NOT NULL a NO INHERIT);
CREATE TABLE t4 (a integer, NOT NULL z);
CREATE TABLE t4 (a integer, NOT NULL a DEFERRABLE);
CREATE TABLE t4 (a integer, NOT NULL a NOT ENFORCED);
CREATE TABLE t4 (a integer, NOT NULL a INITIALLY DEFERRED);
CREATE TABLE t4 (a integer, NOT NULL ctid);
CREATE TABLE t5 (a integer, NOT NULL a NOT VALID, b integer, NOT NULL b, NOT NULL b);
CREATE TABLE t6 (a integer, CONSTRAINT x NOT NULL a, b integer, CONSTRAINT x NOT NULL b);
CREATE TABLE t6 (a integer, CONSTRAINT x CHECK (a > 0), CONSTRAINT x NOT NULL a);
CREATE TABLE t7 (a integer NULL, NOT NULL a);
CREATE TABLE t8 (a integer, NOT NULL a, CONSTRAINT nn UNIQUE (a), CONSTRAINT t8_a_not_null CHECK (a > 1));
SELECT conrelid::regclass::text AS rel, conname, contype, conkey, connoinherit, conislocal, coninhcount
FROM pg_constraint WHERE conrelid::regclass::text LIKE 't_' ORDER BY 1, 2;
CREATE TABLE p (a integer, b integer) PARTITION BY LIST (a);
CREATE TABLE p2 (a integer, NOT NULL a NO INHERIT) PARTITION BY LIST (a);
CREATE TABLE p2 (a integer NOT NULL NO INHERIT) PARTITION BY LIST (a);
CREATE TABLE pc PARTITION OF p (CONSTRAINT pc_nn NOT NULL b) FOR VALUES IN (1);
CREATE TABLE pd PARTITION OF p (NOT NULL z) FOR VALUES IN (2);
CREATE TABLE par (a integer CONSTRAINT par_nn NOT NULL, b integer);
CREATE TABLE ch1 (NOT NULL a NO INHERIT) INHERITS (par);
CREATE TABLE ch1 (a integer NOT NULL NO INHERIT) INHERITS (par);
CREATE TABLE ch2 (CONSTRAINT other NOT NULL a, NOT NULL b) INHERITS (par);
CREATE TABLE ch3 (NOT NULL b NO INHERIT) INHERITS (par);
CREATE TABLE gc (c integer) INHERITS (ch3);
SELECT conrelid::regclass::text AS rel, conname, contype, conkey, connoinherit, conislocal, coninhcount
FROM pg_constraint WHERE conrelid IN ('pc'::regclass, 'ch2'::regclass, 'ch3'::regclass, 'gc'::regclass) ORDER BY 1, 2;
SELECT attrelid::regclass::text, attname, attnotnull FROM pg_attribute
WHERE attrelid IN ('t1'::regclass, 'ch3'::regclass, 'gc'::regclass) AND attnum > 0 ORDER BY 1, attnum;
CREATE TABLE a1 (a integer, b integer, c integer);
INSERT INTO a1 VALUES (1, NULL, 1);
ALTER TABLE a1 ADD CONSTRAINT a1_x NOT NULL a;
ALTER TABLE a1 ADD CONSTRAINT a1_y NOT NULL a;
ALTER TABLE a1 ADD NOT NULL a;
ALTER TABLE a1 ADD CONSTRAINT a1_x NOT NULL a;
ALTER TABLE a1 ADD CONSTRAINT a1_x NOT NULL c;
ALTER TABLE a1 ADD NOT NULL z;
ALTER TABLE a1 ADD NOT NULL a NO INHERIT;
ALTER TABLE a1 ADD CONSTRAINT a1_x NOT NULL a NO INHERIT;
ALTER TABLE a1 ADD NOT NULL b;
ALTER TABLE a1 ADD NOT NULL c DEFERRABLE;
ALTER TABLE a1 ADD NOT NULL c NO INHERIT;
ALTER TABLE a1 ADD CONSTRAINT a1_c NOT NULL c, ADD CONSTRAINT a1_c CHECK (c > 0);
ALTER TABLE a1 ADD NOT NULL ctid;
CREATE TABLE a1c () INHERITS (a1);
ALTER TABLE a1 ADD CONSTRAINT a1_c2 CHECK (c > 0), ADD NOT NULL b;
SELECT conrelid::regclass::text AS rel, conname, contype, conkey, connoinherit, conislocal, coninhcount
FROM pg_constraint WHERE conrelid IN ('a1'::regclass, 'a1c'::regclass) ORDER BY 1, 2;
CREATE TABLE a2 (a integer, b integer);
CREATE TABLE a2c () INHERITS (a2);
ALTER TABLE a2 ADD CONSTRAINT a2_n NOT NULL a;
ALTER TABLE a2 ADD NOT NULL b NO INHERIT;
ALTER TABLE a2c ADD CONSTRAINT a2c_own NOT NULL b;
ALTER TABLE a2c ADD CONSTRAINT a2c_own NOT NULL a;
ALTER TABLE ONLY a2 ADD NOT NULL b;
SELECT conrelid::regclass::text AS rel, conname, contype, conkey, connoinherit, conislocal, coninhcount
FROM pg_constraint WHERE conrelid IN ('a2'::regclass, 'a2c'::regclass) ORDER BY 1, 2;
SELECT pg_get_constraintdef(oid), conname FROM pg_constraint WHERE conrelid = 'a2'::regclass ORDER BY 2;
CREATE TABLE a3 (a integer, b integer) PARTITION BY LIST (a);
CREATE TABLE a3p PARTITION OF a3 FOR VALUES IN (1);
ALTER TABLE a3 ADD NOT NULL b NO INHERIT;
ALTER TABLE a3 ADD CONSTRAINT a3_b NOT NULL b;
ALTER TABLE a3p ADD CONSTRAINT a3p_a NOT NULL a;
SELECT conrelid::regclass::text AS rel, conname, contype, conkey, connoinherit, conislocal, coninhcount
FROM pg_constraint WHERE conrelid IN ('a3'::regclass, 'a3p'::regclass) ORDER BY 1, 2;
CREATE TABLE c1 (a integer CONSTRAINT x NOT NULL CONSTRAINT y NOT NULL);
CREATE TABLE c1 (a integer NOT NULL NO INHERIT NOT NULL);
CREATE TABLE c2 (NOT NULL a, a integer, EXCLUDE USING btree (a WITH =));
SELECT conname, contype FROM pg_constraint WHERE conrelid = 'c2'::regclass ORDER BY 1;
