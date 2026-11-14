-- A foreign key may reference columns covered only by CREATE UNIQUE INDEX.
-- PostgreSQL's transformFkeyCheckAttrs picks, in OID order, the first valid,
-- immediate, unique, non-partial index without expressions whose key columns
-- are exactly the referenced ones in any order, constraint or not, and
-- records it as conindid.
CREATE TABLE parent (a int, b int, c text);
CREATE UNIQUE INDEX parent_idx ON parent (a);
CREATE TABLE child (x int CONSTRAINT fk REFERENCES parent (a));
INSERT INTO parent VALUES (1, 1, 'x');
INSERT INTO child VALUES (1), (NULL);
INSERT INTO child VALUES (2);
SELECT conname, conindid::regclass, pg_get_constraintdef(oid), conpfeqop, conppeqop, conffeqop
  FROM pg_constraint WHERE conrelid = 'child'::regclass;
SELECT pg_describe_object(refclassid, refobjid, refobjsubid), deptype FROM pg_depend
  WHERE classid = 'pg_constraint'::regclass
    AND objid = (SELECT oid FROM pg_constraint WHERE conname = 'fk') ORDER BY 1;
\d parent
\d child
-- Unusable indexes: partial, expression, not unique, deferrable.
CREATE TABLE p3 (a int, b int);
CREATE UNIQUE INDEX p3_partial ON p3 (a) WHERE a > 0;
CREATE UNIQUE INDEX p3_expr ON p3 ((a + 1));
CREATE INDEX p3_plain ON p3 (b);
CREATE TABLE c3 (a int REFERENCES p3 (a));
CREATE TABLE c3 (b int REFERENCES p3 (b));
ALTER TABLE p3 ADD CONSTRAINT p3_b UNIQUE (b) DEFERRABLE;
CREATE TABLE c3 (b int REFERENCES p3 (b));
CREATE UNIQUE INDEX p3_b_idx ON p3 (b);
CREATE TABLE c3 (b int REFERENCES p3 (b));
-- INCLUDE columns are not key columns; the referenced list has no duplicates.
CREATE UNIQUE INDEX p3_incl ON p3 (a) INCLUDE (b);
CREATE TABLE c3a (a int REFERENCES p3 (a));
CREATE TABLE c3b (a int, FOREIGN KEY (a, a) REFERENCES p3 (a, a));
CREATE TABLE c3b (a int, FOREIGN KEY (a) REFERENCES p3 (a, b));
SELECT conrelid::regclass, conindid::regclass FROM pg_constraint
  WHERE confrelid = 'p3'::regclass ORDER BY 1::text;
-- Column order and NULLS NOT DISTINCT do not matter; the oldest index wins.
CREATE TABLE p4 (a int, b int);
CREATE UNIQUE INDEX p4_ba ON p4 (b, a) NULLS NOT DISTINCT;
CREATE UNIQUE INDEX p4_ab ON p4 (a, b);
ALTER TABLE p4 ADD CONSTRAINT p4_uq UNIQUE (a, b);
CREATE TABLE c4 (a int, b int, FOREIGN KEY (a, b) REFERENCES p4 (a, b) ON UPDATE CASCADE ON DELETE SET NULL (b));
SELECT conname, conindid::regclass, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'c4'::regclass;
-- A constraint created before the index is chosen over it, and after it not.
CREATE TABLE p5 (a int);
ALTER TABLE p5 ADD CONSTRAINT p5_uq UNIQUE (a);
CREATE UNIQUE INDEX p5_idx ON p5 (a);
CREATE TABLE c5 (a int REFERENCES p5 (a));
CREATE TABLE p6 (a int);
CREATE UNIQUE INDEX p6_idx ON p6 (a);
ALTER TABLE p6 ADD CONSTRAINT p6_uq UNIQUE (a);
CREATE TABLE c6 (a int REFERENCES p6 (a));
SELECT conrelid::regclass, conindid::regclass FROM pg_constraint
  WHERE conrelid IN ('c5'::regclass, 'c6'::regclass) ORDER BY 1::text;
-- A key without referenced columns still uses the primary key.
CREATE TABLE r (a int UNIQUE, b int PRIMARY KEY);
CREATE UNIQUE INDEX r_b ON r (b);
CREATE TABLE rc (b int REFERENCES r);
SELECT conindid::regclass FROM pg_constraint WHERE conrelid = 'rc'::regclass;
-- A self-reference to keys the same statement creates.
CREATE TABLE s1 (a int UNIQUE, b int PRIMARY KEY, c int REFERENCES s1 (a), d int REFERENCES s1);
CREATE TABLE s2 (a int, b int, CONSTRAINT s2_u1 UNIQUE (a, b), CONSTRAINT s2_u2 UNIQUE (b, a),
  FOREIGN KEY (b, a) REFERENCES s2 (b, a));
CREATE TABLE s3 (a int, b int);
CREATE UNIQUE INDEX s3_a ON s3 (a);
ALTER TABLE s3 ADD CONSTRAINT s3_u UNIQUE (a), ADD CONSTRAINT s3_fk FOREIGN KEY (b) REFERENCES s3 (a);
SELECT conrelid::regclass, conname, conindid::regclass FROM pg_constraint
  WHERE contype = 'f' AND conrelid IN ('s1'::regclass, 's2'::regclass, 's3'::regclass)
  ORDER BY 1::text, 2;
-- The index is checked before privileges and the column counts.
CREATE TABLE np (a int, b int);
CREATE TABLE nc (a int, FOREIGN KEY (a) REFERENCES np (a, b));
CREATE TABLE nc (a int, b int, FOREIGN KEY (a, b) REFERENCES np (a));
CREATE UNIQUE INDEX np_a ON np (a);
CREATE TABLE nc (a int, b int, FOREIGN KEY (a, b) REFERENCES np (a));
-- Existing rows are validated, and actions run through the index.
CREATE TABLE v (a int, b text);
CREATE UNIQUE INDEX v_a ON v (a);
INSERT INTO v VALUES (1, 'x'), (2, 'y');
CREATE TABLE vc (a int, n int);
INSERT INTO vc VALUES (1, 1), (3, 3);
ALTER TABLE vc ADD CONSTRAINT vc_fk FOREIGN KEY (a) REFERENCES v (a) ON DELETE CASCADE ON UPDATE CASCADE;
DELETE FROM vc WHERE a = 3;
ALTER TABLE vc ADD CONSTRAINT vc_fk FOREIGN KEY (a) REFERENCES v (a) ON DELETE CASCADE ON UPDATE CASCADE;
UPDATE v SET a = 10 WHERE a = 1;
DELETE FROM v WHERE a = 2;
SELECT * FROM vc ORDER BY a;
\d vc
-- A partitioned unique index can be referenced.
CREATE TABLE pp (a int, b int) PARTITION BY RANGE (a);
CREATE TABLE pp1 PARTITION OF pp FOR VALUES FROM (0) TO (10);
CREATE UNIQUE INDEX pp_idx ON pp (a);
CREATE TABLE pc (a int REFERENCES pp (a));
INSERT INTO pp VALUES (1, 1);
INSERT INTO pc VALUES (1);
INSERT INTO pc VALUES (2);
SELECT conname, conindid::regclass FROM pg_constraint WHERE conrelid = 'pc'::regclass AND conparentid = 0;
\d pc
DROP TABLE pc, pp, vc, v, np, s1, s2, s3, rc, r;
DROP TABLE c6, p6, c5, p5, c4, p4, c3, c3a, p3, child, parent;
