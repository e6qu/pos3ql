-- A foreign key that references a partitioned table gets one pg_constraint
-- row, with its own two RI action triggers, per referenced partition. Each
-- clone's name is chosen against the namespace when it is made.
CREATE TABLE pk (a int PRIMARY KEY) PARTITION BY RANGE (a);
CREATE TABLE pk1 PARTITION OF pk FOR VALUES FROM (0) TO (10);
CREATE TABLE pk2 PARTITION OF pk FOR VALUES FROM (10) TO (20) PARTITION BY RANGE (a);
CREATE TABLE pk21 PARTITION OF pk2 FOR VALUES FROM (10) TO (15);
CREATE TABLE taken (a int CONSTRAINT pc_a_fkey_2 CHECK (a > 0));
CREATE TABLE pc (a int REFERENCES pk);
CREATE TABLE fr (a int REFERENCES pk) PARTITION BY LIST (a);
CREATE TABLE fr1 PARTITION OF fr FOR VALUES IN (1);
SELECT conname, conrelid::regclass, confrelid::regclass, conindid::regclass,
  (SELECT p.conname FROM pg_constraint p WHERE p.oid = c.conparentid) AS parent,
  conislocal, coninhcount, connoinherit, confkey, pg_get_constraintdef(oid)
FROM pg_constraint c WHERE contype = 'f' ORDER BY oid;
SELECT tgrelid::regclass, tgfoid::regproc, tgtype,
  (SELECT conname FROM pg_constraint WHERE oid = tgconstraint) AS constraint_name,
  tgconstrrelid::regclass, tgconstrindid::regclass, tgparentid <> 0 AS has_parent,
  (SELECT p.tgrelid::regclass FROM pg_trigger p WHERE p.oid = t.tgparentid) AS parent_on
FROM pg_trigger t WHERE tgisinternal
ORDER BY tgrelid::regclass::text, constraint_name, tgtype, tgfoid;
SELECT regexp_replace(pg_describe_object(classid, objid, objsubid), '_\d+ on', '_N on') AS object,
  pg_describe_object(refclassid, refobjid, refobjsubid) AS referenced, deptype
FROM pg_depend
WHERE (classid = 'pg_constraint'::regclass AND objid IN (SELECT oid FROM pg_constraint WHERE contype = 'f'))
   OR (classid = 'pg_trigger'::regclass AND objid IN (SELECT oid FROM pg_trigger WHERE tgisinternal))
ORDER BY 1, 2, 3;
\d pk1
-- Each referenced partition reports violations under its own clone.
INSERT INTO pk VALUES (21);
INSERT INTO pk VALUES (1), (12);
INSERT INTO pc VALUES (1), (12);
INSERT INTO fr VALUES (1);
DELETE FROM pk WHERE a = 1;
DELETE FROM pk21 WHERE a = 12;
UPDATE pk SET a = 2 WHERE a = 1;
-- Partitions added later get clones; detaching removes them, and fails
-- while a row still references the partition.
CREATE TABLE pk3 PARTITION OF pk FOR VALUES FROM (20) TO (30);
INSERT INTO pk VALUES (21);
INSERT INTO pc VALUES (21);
ALTER TABLE pk DETACH PARTITION pk3;
ALTER TABLE pk DETACH PARTITION pk1;
DELETE FROM pc WHERE a = 1;
DELETE FROM fr;
ALTER TABLE pk DETACH PARTITION pk1;
ALTER TABLE pk DETACH PARTITION pk1;
ALTER TABLE pk2 DETACH PARTITION pk3;
CREATE TABLE late (a int CONSTRAINT pc_a_fkey_1 CHECK (a > 0));
ALTER TABLE pk ATTACH PARTITION pk1 FOR VALUES FROM (0) TO (10);
SELECT conname, confrelid::regclass,
  (SELECT p.conname FROM pg_constraint p WHERE p.oid = c.conparentid) AS parent
FROM pg_constraint c WHERE contype = 'f' ORDER BY oid;
ALTER TABLE pc ADD CONSTRAINT pc_a_fkey_3 CHECK (a > 0);
ALTER TABLE pc DROP CONSTRAINT pc_a_fkey_1;
-- Dropping a referenced partition drops the whole key.
DROP TABLE pk3;
DROP TABLE pk3 CASCADE;
SELECT conname, conrelid::regclass, confrelid::regclass
FROM pg_constraint WHERE contype = 'f' ORDER BY oid;
SELECT count(*) FROM pg_trigger WHERE tgisinternal;
ALTER TABLE pc ADD FOREIGN KEY (a) REFERENCES pk;
SELECT conname, confrelid::regclass FROM pg_constraint WHERE contype = 'f' AND conrelid = 'pc'::regclass ORDER BY oid;
ALTER TABLE pc DROP CONSTRAINT pc_a_fkey;
SELECT conname FROM pg_constraint WHERE contype = 'f' AND conrelid = 'pc'::regclass;
