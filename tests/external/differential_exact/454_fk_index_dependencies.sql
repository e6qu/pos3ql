-- A foreign key depends on the unique index it references (conindid): the
-- index cannot be dropped without CASCADE, whether a constraint owns it or
-- not, and a rebuild or rename keeps the dependency.
CREATE TABLE parent (a int, b int, c text);
CREATE UNIQUE INDEX parent_idx ON parent (a);
CREATE TABLE child (x int CONSTRAINT fk REFERENCES parent (a));
DROP INDEX parent_idx;
DROP INDEX parent_idx RESTRICT;
ALTER INDEX parent_idx RENAME TO parent_idx2;
SELECT conindid::regclass FROM pg_constraint WHERE conname = 'fk';
REINDEX INDEX parent_idx2;
REINDEX TABLE parent;
REINDEX INDEX CONCURRENTLY parent_idx2;
REINDEX TABLE CONCURRENTLY parent;
SELECT conindid::regclass FROM pg_constraint WHERE conname = 'fk';
DROP INDEX parent_idx2;
ALTER TABLE parent DROP COLUMN a;
ALTER TABLE parent ALTER COLUMN a TYPE bigint;
SELECT conindid::regclass, pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'fk';
DROP INDEX parent_idx2 CASCADE;
SELECT count(*) FROM pg_constraint WHERE conname = 'fk';
INSERT INTO child VALUES (5);
\d parent
-- Several dependents, and a list of indexes resolved before any is dropped.
CREATE TABLE p4 (a int, b int);
CREATE UNIQUE INDEX p4_ba ON p4 (b, a);
CREATE UNIQUE INDEX p4_ab ON p4 (a, b);
CREATE TABLE c4 (a int, b int, FOREIGN KEY (a, b) REFERENCES p4 (a, b));
CREATE TABLE d4 (a int, b int, FOREIGN KEY (b, a) REFERENCES p4 (b, a));
DROP INDEX p4_ab;
DROP INDEX p4_ba;
DROP INDEX p4_ba, p4_ab;
DROP INDEX p4_ab, p4_missing;
DROP INDEX p4_ba CASCADE;
SELECT count(*) FROM pg_constraint WHERE confrelid = 'p4'::regclass;
-- Dropping a key constraint drops only the keys created against its index.
CREATE TABLE p5 (a int);
ALTER TABLE p5 ADD CONSTRAINT p5_uq UNIQUE (a);
CREATE UNIQUE INDEX p5_idx ON p5 (a);
CREATE TABLE c5 (a int REFERENCES p5 (a));
CREATE TABLE d5 (a int REFERENCES p5 (a));
DROP INDEX p5_idx;
ALTER TABLE p5 DROP CONSTRAINT p5_uq;
ALTER TABLE p5 DROP CONSTRAINT p5_uq CASCADE;
CREATE TABLE p6 (a int);
CREATE UNIQUE INDEX p6_idx ON p6 (a);
ALTER TABLE p6 ADD CONSTRAINT p6_uq UNIQUE (a);
CREATE TABLE c6 (a int REFERENCES p6 (a));
ALTER TABLE p6 DROP CONSTRAINT p6_uq;
DROP INDEX p6_idx;
-- A referenced column, or an INCLUDE column of the referenced index.
CREATE TABLE u1 (a int, b int, c int);
CREATE UNIQUE INDEX u1_a ON u1 (a) INCLUDE (c);
CREATE TABLE u2 (a int REFERENCES u1 (a));
CREATE VIEW u1v AS SELECT a FROM u1;
ALTER TABLE u1 DROP COLUMN a;
ALTER TABLE u1 DROP COLUMN c;
ALTER TABLE u1 DROP COLUMN b;
ALTER TABLE u1 DROP COLUMN c CASCADE;
DROP VIEW u1v;
CREATE UNIQUE INDEX u1_a2 ON u1 (a);
CREATE TABLE u3 (a int REFERENCES u1 (a));
ALTER TABLE u1 DROP COLUMN a CASCADE;
CREATE TABLE t (a int UNIQUE, b int REFERENCES t (a));
ALTER TABLE t DROP COLUMN a;
ALTER TABLE t DROP COLUMN b;
\d t
-- The index becomes a constraint's and keeps its dependents.
CREATE TABLE w (a int);
CREATE UNIQUE INDEX w_a ON w (a);
CREATE TABLE wc (a int REFERENCES w (a));
ALTER TABLE w ADD CONSTRAINT w_a UNIQUE USING INDEX w_a;
SELECT conindid::regclass FROM pg_constraint WHERE conrelid = 'wc'::regclass;
ALTER TABLE w DROP CONSTRAINT w_a;
-- Partitioned tables on either side.
CREATE TABLE pp (a int, b int) PARTITION BY RANGE (a);
CREATE TABLE pp1 PARTITION OF pp FOR VALUES FROM (0) TO (10);
CREATE UNIQUE INDEX pp_idx ON pp (a);
CREATE TABLE pc (a int REFERENCES pp (a));
DROP INDEX pp1_a_idx;
DROP INDEX pp_idx;
DROP INDEX pp_idx CASCADE;
\d pc
CREATE TABLE q (a int);
CREATE UNIQUE INDEX q_idx ON q (a);
CREATE TABLE qc (a int REFERENCES q (a)) PARTITION BY LIST (a);
CREATE TABLE qc1 PARTITION OF qc FOR VALUES IN (1);
SELECT conrelid::regclass, conname, conindid::regclass, conparentid <> 0 FROM pg_constraint
  WHERE confrelid = 'q'::regclass ORDER BY 1::text;
SELECT pg_describe_object(classid, objid, objsubid), pg_describe_object(refclassid, refobjid, refobjsubid), deptype
  FROM pg_depend WHERE classid = 'pg_constraint'::regclass
    AND objid IN (SELECT oid FROM pg_constraint WHERE confrelid = 'q'::regclass) ORDER BY 1, 2;
DROP INDEX q_idx;
DROP TABLE q;
DROP INDEX q_idx CASCADE;
SELECT count(*) FROM pg_constraint WHERE confrelid = 'q'::regclass;
ALTER TABLE q ADD CONSTRAINT q_uq UNIQUE (a);
ALTER TABLE qc ADD FOREIGN KEY (a) REFERENCES q (a);
ALTER TABLE q DROP CONSTRAINT q_uq;
ALTER TABLE q DROP CONSTRAINT q_uq CASCADE;
ALTER TABLE q ADD CONSTRAINT q_uq UNIQUE (a);
ALTER TABLE qc ADD FOREIGN KEY (a) REFERENCES q (a);
ALTER TABLE q DROP COLUMN a CASCADE;
CREATE UNIQUE INDEX q_idx ON q (b);
ALTER TABLE q ADD COLUMN b int;
CREATE UNIQUE INDEX q_idx ON q (b);
CREATE TABLE qc2 PARTITION OF qc FOR VALUES IN (2);
ALTER TABLE qc ADD FOREIGN KEY (a) REFERENCES q (b);
SELECT conrelid::regclass, conname, conindid::regclass FROM pg_constraint
  WHERE confrelid = 'q'::regclass ORDER BY 1::text;
DROP TABLE q CASCADE;
DROP TABLE qc, pc, pp, wc, w, t, u3, u2, u1;
DROP TABLE c6, p6, d5, c5, p5, d4, c4, p4, child, parent;
