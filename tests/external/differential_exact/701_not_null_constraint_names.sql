-- CREATE TABLE stores its CHECK constraints, then its NOT NULL constraints
-- one at a time (AddRelationNotNullConstraints): a declared name repeating
-- a stored one fails pg_constraint's unique index, whose DETAIL names the new
-- table's OID; a generated name avoids the schema's constraints and earlier
-- NOT NULL names only; an inherited name gives way to a name the table
-- already uses; LIKE copies names. ALTER TABLE checks names first (42710).
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/heap.c
CREATE TABLE t1 (id integer, b integer, CONSTRAINT x CHECK (id > 0));
ALTER TABLE t1 ALTER b SET NOT NULL;
ALTER TABLE t1 ADD COLUMN c integer CONSTRAINT x NOT NULL;
ALTER TABLE t1 ADD COLUMN d integer CONSTRAINT x CHECK (d > 0);
CREATE TABLE t2 (a integer CONSTRAINT x NOT NULL, b integer CONSTRAINT x NOT NULL,
  CONSTRAINT x CHECK (a > 0));
CREATE TABLE t3 (a integer CONSTRAINT x NOT NULL, b integer CONSTRAINT y NOT NULL,
  CONSTRAINT y CHECK (a > 0), CONSTRAINT x UNIQUE (a));
CREATE TABLE t4 (a integer CONSTRAINT x NOT NULL, CONSTRAINT x CHECK (a > 0))
  PARTITION BY RANGE (a);
CREATE TABLE p (a integer, CONSTRAINT x CHECK (a > 0));
CREATE TABLE c2 (a integer CONSTRAINT x NOT NULL) INHERITS (p);
CREATE TABLE pp (a integer CONSTRAINT x NOT NULL) PARTITION BY LIST (a);
CREATE TABLE pc PARTITION OF pp (CONSTRAINT x CHECK (a > 0)) FOR VALUES IN (1);
SELECT conrelid::regclass::text, conname, contype FROM pg_constraint
WHERE conrelid IN ('pp'::regclass, 'pc'::regclass) ORDER BY 1, 2;
CREATE TABLE t6 (a integer CONSTRAINT x NOT NULL, CONSTRAINT x UNIQUE (a));
CREATE TABLE t7 (a integer CONSTRAINT x NOT NULL, b integer);
ALTER TABLE t7 ADD COLUMN c integer CONSTRAINT x NOT NULL;
ALTER TABLE t7 ADD COLUMN c integer CONSTRAINT y NOT NULL, ADD COLUMN d integer CONSTRAINT y NOT NULL;
ALTER TABLE t7 ADD COLUMN c integer CONSTRAINT y NOT NULL, ADD CONSTRAINT y CHECK (a > 0);
ALTER TABLE t7 ADD CONSTRAINT y CHECK (a > 0), ADD COLUMN c integer CONSTRAINT y NOT NULL;
CREATE TABLE pp2 (a integer, CONSTRAINT x CHECK (a > 0)) PARTITION BY LIST (a);
CREATE TABLE pc2 PARTITION OF pp2 (a CONSTRAINT x NOT NULL) FOR VALUES IN (1);
CREATE TABLE q1 (a integer CONSTRAINT x NOT NULL, b integer CONSTRAINT q2_b_not_null NOT NULL);
CREATE TABLE q2 (CONSTRAINT x CHECK (a > 0)) INHERITS (q1);
CREATE TABLE q3 (CONSTRAINT x CHECK (a > 0), CONSTRAINT q3_a_not_null CHECK (a > 1)) INHERITS (q1);
CREATE TABLE q4 (CONSTRAINT q2_b_not_null CHECK (b > 0)) INHERITS (q1);
SELECT conrelid::regclass::text, conname, contype FROM pg_constraint
WHERE conrelid IN ('q2'::regclass, 'q3'::regclass, 'q4'::regclass) ORDER BY 1, 2;
CREATE TABLE r1 (a integer CONSTRAINT r2_a_not_null CHECK (a > 0));
CREATE TABLE r2 (a integer NOT NULL);
CREATE TABLE r3 (a integer NOT NULL, b integer CONSTRAINT r3_a_not_null NOT NULL);
CREATE TABLE r4 (a integer NOT NULL, CONSTRAINT r4_a_not_null CHECK (a > 0));
CREATE TABLE r5 (a integer NOT NULL, b integer CONSTRAINT r5_a_not_null UNIQUE);
CREATE TABLE r6 (a integer NOT NULL, b integer CONSTRAINT r6_a_not_null1 NOT NULL);
CREATE TABLE r8 (b integer CONSTRAINT r6_a_not_null NOT NULL,
  CONSTRAINT r6_a_not_null CHECK (b > 0));
CREATE TABLE l1 (a integer CONSTRAINT custom_nn NOT NULL, b integer NOT NULL);
CREATE TABLE l2 (LIKE l1);
CREATE TABLE l3 (LIKE l1, CONSTRAINT custom_nn CHECK (a > 0));
CREATE TABLE l4 (LIKE l1, c integer CONSTRAINT custom_nn NOT NULL);
CREATE TABLE p2 (a integer CONSTRAINT pnn NOT NULL);
CREATE TABLE c3 (a integer NOT NULL) INHERITS (p2);
CREATE TABLE c4 () INHERITS (p2);
CREATE TABLE c5 (LIKE c4);
CREATE TABLE r9 (a integer CONSTRAINT r10_b_not_null CHECK (a > 0));
CREATE TABLE r10 (a integer);
ALTER TABLE r10 ADD COLUMN b integer NOT NULL;
ALTER TABLE r10 ALTER COLUMN a SET NOT NULL;
SELECT conrelid::regclass::text, conname, contype FROM pg_constraint
WHERE conrelid::regclass::text IN ('r2', 'r4', 'r6', 'l2', 'c3', 'c4', 'c5', 'r10')
ORDER BY 1, 2;
DROP TABLE t1, p, pp, t7, pp2, q1, r1, r2 CASCADE;
DROP TABLE r4, r6, l1, l2, p2, c5, r9, r10 CASCADE;
