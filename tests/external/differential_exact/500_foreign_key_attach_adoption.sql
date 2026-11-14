-- Attaching a partition adopts its equivalent foreign key under the
-- parent's (tryAttachPartitionForeignKey): the key keeps its name, loses its
-- action triggers and goes with the parent's key; detaching gives it new
-- action triggers. A key's check triggers follow the clones it gets for a
-- partitioned referenced table, a NOT ENFORCED key has no triggers, and a
-- parent key's rename does not reach its partitions' keys.
-- section 1
CREATE TABLE a0_pk (id int PRIMARY KEY);
CREATE TABLE a0_fp (id int, ref int, CONSTRAINT fp_ref_fk FOREIGN KEY (ref) REFERENCES a0_pk) PARTITION BY LIST (id);
CREATE TABLE a0_fp_1 (id int, ref int, CONSTRAINT own_fk FOREIGN KEY (ref) REFERENCES a0_pk);
SELECT conname, oid > 0 FROM pg_constraint WHERE conrelid = 'a0_fp_1'::regclass ORDER BY oid;
ALTER TABLE a0_fp ATTACH PARTITION a0_fp_1 FOR VALUES IN (1);
SELECT conname, conrelid::regclass, conparentid::regclass::text <> '-' AS has_parent, conislocal, coninhcount FROM pg_constraint WHERE contype = 'f' ORDER BY oid;
SELECT c.conname, t.tgfoid::regproc, rank() OVER (ORDER BY t.oid) FROM pg_trigger t JOIN pg_constraint c ON c.oid = t.tgconstraint WHERE c.contype = 'f' ORDER BY t.oid;
\d a0_fp_1
ALTER TABLE a0_fp DETACH PARTITION a0_fp_1;
SELECT conname, conrelid::regclass, conislocal, coninhcount FROM pg_constraint WHERE contype = 'f' ORDER BY oid;
-- partitioned referenced table: trigger order
CREATE TABLE a0_rp (id int PRIMARY KEY) PARTITION BY LIST (id);
CREATE TABLE a0_rp_1 PARTITION OF a0_rp FOR VALUES IN (1);
CREATE TABLE a0_rp_2 PARTITION OF a0_rp FOR VALUES IN (2);
CREATE TABLE a0_rc (x int REFERENCES a0_rp);
SELECT c.conname, t.tgrelid::regclass, t.tgfoid::regproc, rank() OVER (ORDER BY t.oid) FROM pg_trigger t JOIN pg_constraint c ON c.oid = t.tgconstraint WHERE c.conrelid = 'a0_rc'::regclass ORDER BY t.oid;
SELECT conname, conrelid::regclass, confrelid::regclass, rank() OVER (ORDER BY oid) FROM pg_constraint WHERE conrelid = 'a0_rc'::regclass ORDER BY oid;
DROP TABLE a0_fp, a0_fp_1, a0_pk, a0_rc, a0_rp, a0_rp_1, a0_rp_2 CASCADE;
-- section 2
CREATE TABLE a1_pk (id int PRIMARY KEY);
CREATE TABLE a1_fp (id int, ref int, CONSTRAINT fp_ref_fk FOREIGN KEY (ref) REFERENCES a1_pk) PARTITION BY LIST (id);
CREATE TABLE a1_fp_1 PARTITION OF a1_fp FOR VALUES IN (1);
CREATE TABLE a1_fp_2 (id int, ref int, CONSTRAINT own_fk FOREIGN KEY (ref) REFERENCES a1_pk);
ALTER TABLE a1_fp ATTACH PARTITION a1_fp_2 FOR VALUES IN (2);
SELECT c.conname, c.conrelid::regclass, t.tgrelid::regclass, t.tgfoid::regproc, t.tgparentid <> 0 AS child, rank() OVER (ORDER BY t.oid) FROM pg_trigger t JOIN pg_constraint c ON c.oid = t.tgconstraint WHERE c.contype = 'f' ORDER BY t.oid;
ALTER TABLE a1_fp DETACH PARTITION a1_fp_1;
ALTER TABLE a1_fp DETACH PARTITION a1_fp_2;
SELECT c.conname, c.conrelid::regclass, t.tgrelid::regclass, t.tgfoid::regproc, t.tgparentid <> 0 AS child, rank() OVER (ORDER BY t.oid) FROM pg_trigger t JOIN pg_constraint c ON c.oid = t.tgconstraint WHERE c.contype = 'f' ORDER BY t.oid;
SELECT conname, conrelid::regclass, conislocal, coninhcount, conparentid <> 0 AS child, rank() OVER (ORDER BY oid) FROM pg_constraint WHERE contype = 'f' ORDER BY oid;
DROP TABLE a1_fp, a1_fp_1, a1_fp_2, a1_pk CASCADE;
-- section 3
CREATE TABLE a2_pk (id int PRIMARY KEY);
CREATE TABLE a2_fp (id int, ref int, CONSTRAINT fp_ref_fk FOREIGN KEY (ref) REFERENCES a2_pk) PARTITION BY LIST (id);
CREATE TABLE a2_fp_1 PARTITION OF a2_fp FOR VALUES IN (1);
ALTER TABLE a2_fp RENAME CONSTRAINT fp_ref_fk TO renamed_fk;
SELECT conname, conrelid::regclass FROM pg_constraint WHERE contype = 'f' ORDER BY oid;
ALTER TABLE a2_fp_1 DROP CONSTRAINT fp_ref_fk;
ALTER TABLE a2_fp DROP CONSTRAINT renamed_fk;
SELECT conname, conrelid::regclass FROM pg_constraint WHERE contype = 'f' ORDER BY oid;
DROP TABLE a2_fp, a2_fp_1, a2_pk CASCADE;
-- section 4
CREATE TABLE a3_rp (id int PRIMARY KEY) PARTITION BY LIST (id);
CREATE TABLE a3_rp_1 PARTITION OF a3_rp FOR VALUES IN (1);
CREATE TABLE a3_rp_2 PARTITION OF a3_rp FOR VALUES IN (2);
INSERT INTO a3_rp VALUES (1), (2);
CREATE TABLE a3_fq (id int, ref int, CONSTRAINT fq_fk FOREIGN KEY (ref) REFERENCES a3_rp) PARTITION BY LIST (id);
CREATE TABLE a3_fq_1 (id int, ref int, CONSTRAINT own1 FOREIGN KEY (ref) REFERENCES a3_rp);
SELECT conname, conrelid::regclass, confrelid::regclass FROM pg_constraint WHERE contype = 'f' ORDER BY oid;
ALTER TABLE a3_fq ATTACH PARTITION a3_fq_1 FOR VALUES IN (1);
SELECT conname, conrelid::regclass, confrelid::regclass, conparentid <> 0 AS child FROM pg_constraint WHERE contype = 'f' ORDER BY oid;
SELECT c.conname, t.tgrelid::regclass, t.tgfoid::regproc, rank() OVER (ORDER BY t.oid) FROM pg_trigger t JOIN pg_constraint c ON c.oid = t.tgconstraint WHERE c.contype = 'f' ORDER BY t.oid;
-- name conflict: the partition has another constraint named like the parent's key
CREATE TABLE a3_fq_2 (id int, ref int, CONSTRAINT fq_fk CHECK (ref > 0));
ALTER TABLE a3_fq ATTACH PARTITION a3_fq_2 FOR VALUES IN (2);
SELECT conname, conrelid::regclass FROM pg_constraint WHERE conrelid = 'a3_fq_2'::regclass ORDER BY oid;
-- enforceability conflict
CREATE TABLE a3_pk (id int PRIMARY KEY);
INSERT INTO a3_pk VALUES (1);
CREATE TABLE a3_fe (id int, ref int, CONSTRAINT fe_fk FOREIGN KEY (ref) REFERENCES a3_pk) PARTITION BY LIST (id);
CREATE TABLE a3_fe_1 (id int, ref int, CONSTRAINT fe_1_fk FOREIGN KEY (ref) REFERENCES a3_pk NOT ENFORCED);
ALTER TABLE a3_fe ATTACH PARTITION a3_fe_1 FOR VALUES IN (1);
-- not valid child under a valid parent is validated
CREATE TABLE a3_fv_1 (id int, ref int);
INSERT INTO a3_fv_1 VALUES (1, 5);
ALTER TABLE a3_fv_1 ADD CONSTRAINT fv_1_fk FOREIGN KEY (ref) REFERENCES a3_pk NOT VALID;
CREATE TABLE a3_fv (id int, ref int, CONSTRAINT fv_fk FOREIGN KEY (ref) REFERENCES a3_pk) PARTITION BY LIST (id);
ALTER TABLE a3_fv ATTACH PARTITION a3_fv_1 FOR VALUES IN (1);
DELETE FROM a3_fv_1;
ALTER TABLE a3_fv ATTACH PARTITION a3_fv_1 FOR VALUES IN (1);
SELECT conname, convalidated, conparentid <> 0 FROM pg_constraint WHERE conrelid = 'a3_fv_1'::regclass;
-- different actions: no adoption
CREATE TABLE a3_fd (id int, ref int, CONSTRAINT fd_fk FOREIGN KEY (ref) REFERENCES a3_pk ON DELETE CASCADE) PARTITION BY LIST (id);
CREATE TABLE a3_fd_1 (id int, ref int, CONSTRAINT fd_1_fk FOREIGN KEY (ref) REFERENCES a3_pk);
ALTER TABLE a3_fd ATTACH PARTITION a3_fd_1 FOR VALUES IN (1);
SELECT conname, conparentid <> 0 FROM pg_constraint WHERE conrelid = 'a3_fd_1'::regclass ORDER BY oid;
-- dropping the parent key drops an adopted key
ALTER TABLE a3_fq DROP CONSTRAINT fq_fk;
SELECT conname, conrelid::regclass FROM pg_constraint WHERE contype = 'f' AND conrelid::regclass::text LIKE 'a3_fq%' ORDER BY oid;
-- multi-level
CREATE TABLE a3_fm (id int, sub int, ref int, CONSTRAINT fm_fk FOREIGN KEY (ref) REFERENCES a3_pk) PARTITION BY LIST (id);
CREATE TABLE a3_fm_1 PARTITION OF a3_fm FOR VALUES IN (1) PARTITION BY LIST (sub);
CREATE TABLE a3_fm_1_1 (id int, sub int, ref int, CONSTRAINT deep_fk FOREIGN KEY (ref) REFERENCES a3_pk);
ALTER TABLE a3_fm_1 ATTACH PARTITION a3_fm_1_1 FOR VALUES IN (1);
SELECT conname, conrelid::regclass, conparentid::regclass FROM (SELECT conname, conrelid, (SELECT p.conrelid FROM pg_constraint p WHERE p.oid = c.conparentid) AS conparentid, oid FROM pg_constraint c WHERE contype = 'f' AND conrelid::regclass::text LIKE 'a3_fm%') s ORDER BY oid;
ALTER TABLE a3_fm DETACH PARTITION a3_fm_1;
SELECT conname, conrelid::regclass, conparentid <> 0 FROM pg_constraint WHERE contype = 'f' AND conrelid::regclass::text LIKE 'a3_fm%' ORDER BY oid;
DROP TABLE a3_fd, a3_fd_1, a3_fe, a3_fe_1, a3_fm, a3_fm_1, a3_fm_1_1, a3_fq, a3_fq_1, a3_fq_2, a3_fv, a3_fv_1, a3_pk, a3_rp, a3_rp_1, a3_rp_2 CASCADE;
-- section 5
CREATE TABLE a4_pk (id int PRIMARY KEY);
INSERT INTO a4_pk VALUES (1);
CREATE TABLE a4_ne (ref int, CONSTRAINT ne_fk FOREIGN KEY (ref) REFERENCES a4_pk NOT ENFORCED);
CREATE TABLE a4_other (ref int REFERENCES a4_pk);
SELECT c.conname, t.tgfoid::regproc, rank() OVER (ORDER BY t.oid) FROM pg_trigger t JOIN pg_constraint c ON c.oid = t.tgconstraint WHERE c.contype = 'f' ORDER BY t.oid;
ALTER TABLE a4_ne ALTER CONSTRAINT ne_fk ENFORCED;
SELECT c.conname, t.tgfoid::regproc, rank() OVER (ORDER BY t.oid) FROM pg_trigger t JOIN pg_constraint c ON c.oid = t.tgconstraint WHERE c.contype = 'f' ORDER BY t.oid;
INSERT INTO a4_ne VALUES (5);
ALTER TABLE a4_ne ALTER CONSTRAINT ne_fk NOT ENFORCED;
SELECT c.conname, t.tgfoid::regproc, rank() OVER (ORDER BY t.oid) FROM pg_trigger t JOIN pg_constraint c ON c.oid = t.tgconstraint WHERE c.contype = 'f' ORDER BY t.oid;
INSERT INTO a4_ne VALUES (5);
DROP TABLE a4_ne, a4_other, a4_pk CASCADE;
