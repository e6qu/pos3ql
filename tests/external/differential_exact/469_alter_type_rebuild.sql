-- ALTER COLUMN ... TYPE drops and re-adds every key, index, NOT NULL, CHECK
-- and foreign key constraint and statistics object over the column, each
-- under a new OID in PostgreSQL's order; their names, comments, CLUSTER and
-- replica identity marks survive, and a partition's copies are re-added by
-- their parent under newly generated names.
CREATE TABLE ar_p (id int PRIMARY KEY, u int UNIQUE, v int);
INSERT INTO ar_p VALUES (1, 1, 1), (2, 2, 2);
CREATE TABLE ar_t (a int, b int, c int,
  CONSTRAINT ar_t_u UNIQUE (b), CONSTRAINT ar_t_chk CHECK (b > 0),
  CONSTRAINT ar_t_chk2 CHECK (a > 0), CONSTRAINT ar_t_pk PRIMARY KEY (a, b),
  CONSTRAINT ar_t_fk FOREIGN KEY (b) REFERENCES ar_p (u));
CREATE INDEX ar_t_bi ON ar_t (b);
CREATE INDEX ar_t_ci ON ar_t (c);
CREATE UNIQUE INDEX ar_t_bu ON ar_t (b) WHERE c IS NULL;
COMMENT ON INDEX ar_t_bi IS 'b index';
COMMENT ON CONSTRAINT ar_t_u ON ar_t IS 'b key';
CREATE STATISTICS ar_t_s ON a, b FROM ar_t;
CREATE ROLE ar_owner;
ALTER STATISTICS ar_t_s OWNER TO ar_owner;
CREATE TABLE ar_r (x int REFERENCES ar_t (b));
INSERT INTO ar_t VALUES (1, 1, NULL);
INSERT INTO ar_r VALUES (1);
CREATE TEMP TABLE ar_before AS
  SELECT 'constraint' AS kind, conname::text AS name, oid::int AS id
    FROM pg_constraint WHERE conrelid IN ('ar_t'::regclass, 'ar_r'::regclass, 'ar_p'::regclass)
  UNION ALL SELECT 'index', relname::text, oid::int FROM pg_class
    WHERE relname LIKE 'ar\_%' AND relkind = 'i'
  UNION ALL SELECT 'statistics', stxname::text, oid::int FROM pg_statistic_ext
    WHERE stxname LIKE 'ar\_%'
  UNION ALL SELECT 'trigger', c.conname || ' ' || t.tgfoid::regproc, t.oid::int
    FROM pg_trigger t JOIN pg_constraint c ON c.oid = t.tgconstraint
    WHERE c.conname IN ('ar_t_fk', 'ar_r_x_fkey', 'ar_p_u_key');
BEGIN;
ALTER TABLE ar_t ALTER COLUMN b TYPE bigint;
ROLLBACK;
ALTER TABLE ar_t ALTER COLUMN b TYPE bigint;
SELECT kind, name, id = (SELECT b.id FROM ar_before b WHERE b.kind = a.kind AND b.name = a.name) AS kept,
       rank() OVER (PARTITION BY kind ORDER BY id) AS position
  FROM (SELECT 'constraint' AS kind, conname::text AS name, oid::int AS id
          FROM pg_constraint WHERE conrelid IN ('ar_t'::regclass, 'ar_r'::regclass, 'ar_p'::regclass)
        UNION ALL SELECT 'index', relname::text, oid::int FROM pg_class
          WHERE relname LIKE 'ar\_%' AND relkind = 'i'
        UNION ALL SELECT 'statistics', stxname::text, oid::int FROM pg_statistic_ext
          WHERE stxname LIKE 'ar\_%'
        UNION ALL SELECT 'trigger', c.conname || ' ' || t.tgfoid::regproc, t.oid::int
          FROM pg_trigger t JOIN pg_constraint c ON c.oid = t.tgconstraint
          WHERE c.conname IN ('ar_t_fk', 'ar_r_x_fkey', 'ar_p_u_key')) AS a
 ORDER BY kind, id;
\d ar_t
SELECT stxname, stxowner::regrole FROM pg_statistic_ext WHERE stxname = 'ar_t_s';
SELECT obj_description('ar_t_bi'::regclass, 'pg_class') AS index_comment,
       obj_description((SELECT oid FROM pg_constraint WHERE conname = 'ar_t_u'), 'pg_constraint') AS key_comment;
SELECT conname, conindid::regclass FROM pg_constraint
 WHERE conname IN ('ar_t_fk', 'ar_r_x_fkey') ORDER BY conname;
INSERT INTO ar_t VALUES (2, 1, 5);
INSERT INTO ar_t VALUES (1, 2, NULL);
INSERT INTO ar_t VALUES (3, 1, NULL);
INSERT INTO ar_t VALUES (3, -1, NULL);
INSERT INTO ar_t VALUES (3, 7, NULL);
INSERT INTO ar_r VALUES (5);
DELETE FROM ar_t WHERE b = 1;
-- A row violating two keys reports the older index; the rebuilt one is now
-- the newer.
CREATE TABLE ar_two (a int, b int, CONSTRAINT ar_two_b UNIQUE (b), CONSTRAINT ar_two_a UNIQUE (a));
INSERT INTO ar_two VALUES (1, 1);
INSERT INTO ar_two VALUES (1, 1);
ALTER TABLE ar_two ALTER COLUMN b TYPE bigint;
INSERT INTO ar_two VALUES (1, 1);
-- CLUSTER and replica identity marks follow the rebuilt index.
CREATE TABLE ar_marks (a int NOT NULL, b int NOT NULL);
CREATE UNIQUE INDEX ar_marks_a ON ar_marks (a);
CREATE UNIQUE INDEX ar_marks_b ON ar_marks (b);
ALTER TABLE ar_marks CLUSTER ON ar_marks_a;
ALTER TABLE ar_marks REPLICA IDENTITY USING INDEX ar_marks_b;
ALTER TABLE ar_marks ALTER COLUMN a TYPE bigint, ALTER COLUMN b TYPE bigint;
SELECT c.relname, i.indisclustered, i.indisreplident FROM pg_index i
  JOIN pg_class c ON c.oid = i.indexrelid WHERE i.indrelid = 'ar_marks'::regclass ORDER BY c.oid;
-- Partitions: the parent's keys and indexes re-add their partition copies,
-- which take generated names; a partition's own index follows.
CREATE TABLE ar_pr (id int, label int, CONSTRAINT ar_pr_pk PRIMARY KEY (id, label),
  CONSTRAINT ar_pr_u UNIQUE (label, id)) PARTITION BY LIST (id);
CREATE TABLE ar_pr_1 PARTITION OF ar_pr FOR VALUES IN (1);
CREATE TABLE ar_pr_2 PARTITION OF ar_pr FOR VALUES IN (2);
CREATE INDEX ar_pr_label_idx ON ar_pr (label);
CREATE INDEX ar_pr_1_own ON ar_pr_1 (label);
ALTER INDEX ar_pr_1_label_idx RENAME TO ar_pr_1_renamed;
ALTER TABLE ar_pr_1 RENAME CONSTRAINT ar_pr_1_pkey TO ar_pr_1_custom;
ALTER TABLE ar_pr ADD CONSTRAINT ar_pr_chk CHECK (label > 0);
ALTER TABLE ar_pr ALTER COLUMN label TYPE bigint;
SELECT c.relname, i.inhparent::regclass FROM pg_class c
  LEFT JOIN pg_inherits i ON i.inhrelid = c.oid
 WHERE c.relname LIKE 'ar\_pr%' AND c.relkind IN ('i', 'I') ORDER BY c.oid;
SELECT conname, conrelid::regclass, conparentid <> 0 AS child FROM pg_constraint
 WHERE conrelid::regclass::text LIKE 'ar\_pr%' ORDER BY oid;
INSERT INTO ar_pr VALUES (1, 1);
INSERT INTO ar_pr VALUES (1, 1);
INSERT INTO ar_pr VALUES (2, 0);
DROP TABLE ar_r, ar_t, ar_p, ar_two, ar_marks, ar_pr;
DROP ROLE ar_owner;
