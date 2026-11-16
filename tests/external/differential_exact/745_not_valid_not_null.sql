-- PostgreSQL 18's NOT VALID not-null constraint: new rows must satisfy it,
-- existing ones are not checked, `pg_attribute.attnotnull` is true but no
-- proof relies on it, and VALIDATE CONSTRAINT or SET NOT NULL checks the
-- stored rows and marks it valid. A valid constraint, a primary key or a
-- second name cannot take it over. DROP CONSTRAINT drops a not-null
-- constraint as DROP NOT NULL does.
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/pg_constraint.c
CREATE TABLE nn745 (a integer, b integer);
INSERT INTO nn745 VALUES (1, NULL), (2, 3);
ALTER TABLE nn745 ADD CONSTRAINT nn745_b NOT NULL b NOT VALID;
SELECT conname, convalidated, conislocal, coninhcount, connoinherit, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'nn745'::regclass;
SELECT attname, attnotnull FROM pg_attribute WHERE attrelid = 'nn745'::regclass AND attnum > 0 ORDER BY attnum;
SELECT * FROM nn745 WHERE b IS NULL;
SELECT count(*) FROM nn745 WHERE b IS NOT NULL;
EXPLAIN (COSTS OFF) SELECT * FROM nn745 WHERE b IS NULL;
INSERT INTO nn745 VALUES (3, NULL);
UPDATE nn745 SET a = 10 WHERE a = 1;
ALTER TABLE nn745 ADD COLUMN c integer DEFAULT 5;
ALTER TABLE nn745 ALTER COLUMN c TYPE bigint;
ALTER TABLE nn745 ALTER COLUMN b SET NOT NULL;
ALTER TABLE nn745 VALIDATE CONSTRAINT nn745_b;
ALTER TABLE nn745 ADD CONSTRAINT nn745_b2 NOT NULL b NOT VALID;
ALTER TABLE nn745 ADD CONSTRAINT nn745_b2 NOT NULL b;
ALTER TABLE nn745 ADD NOT NULL b;
ALTER TABLE nn745 ADD NOT NULL b NOT VALID;
ALTER TABLE nn745 ADD PRIMARY KEY (b);
UPDATE nn745 SET b = 1 WHERE b IS NULL;
UPDATE nn745 SET b = NULL WHERE a = 2;
ALTER TABLE nn745 ALTER COLUMN b SET NOT NULL;
SELECT conname, convalidated, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'nn745'::regclass;
ALTER TABLE nn745 ADD PRIMARY KEY (b);
CREATE TABLE p745 (a integer, b integer);
CREATE TABLE c745 () INHERITS (p745);
INSERT INTO c745 VALUES (1, NULL);
ALTER TABLE p745 ADD CONSTRAINT p745_b NOT NULL b NOT VALID;
SELECT conrelid::regclass::text AS rel, conname, convalidated, conislocal, coninhcount FROM pg_constraint WHERE contype = 'n' AND conrelid IN ('p745'::regclass, 'c745'::regclass) ORDER BY rel;
ALTER TABLE c745 DROP CONSTRAINT p745_b;
ALTER TABLE p745 VALIDATE CONSTRAINT p745_b;
DELETE FROM c745;
ALTER TABLE p745 VALIDATE CONSTRAINT p745_b;
SELECT conrelid::regclass::text AS rel, conname, convalidated FROM pg_constraint WHERE contype = 'n' AND conrelid IN ('p745'::regclass, 'c745'::regclass) ORDER BY rel;
ALTER TABLE p745 DROP CONSTRAINT p745_b;
SELECT count(*) FROM pg_constraint WHERE contype = 'n' AND conrelid IN ('p745'::regclass, 'c745'::regclass);
CREATE TABLE q745 (a integer, b integer);
ALTER TABLE q745 ADD CONSTRAINT q745_b NOT NULL b NOT VALID;
ALTER TABLE q745 ALTER COLUMN b DROP NOT NULL;
SELECT count(*) FROM pg_constraint WHERE conrelid = 'q745'::regclass;
ALTER TABLE q745 ADD NOT NULL b NOT VALID;
SELECT conname, convalidated FROM pg_constraint WHERE conrelid = 'q745'::regclass;
ALTER TABLE q745 DROP CONSTRAINT q745_b_not_null;
ALTER TABLE q745 DROP CONSTRAINT q745_b_not_null;
ALTER TABLE q745 DROP CONSTRAINT IF EXISTS q745_b_not_null;
ALTER TABLE q745 ADD NOT NULL b;
ALTER TABLE q745 DROP CONSTRAINT q745_b_not_null;
SELECT attnotnull FROM pg_attribute WHERE attrelid = 'q745'::regclass AND attname = 'b';
CREATE TABLE r745 (a integer NOT NULL, b integer);
ALTER TABLE r745 ADD NOT NULL a NOT VALID;
SELECT conname, convalidated FROM pg_constraint WHERE conrelid = 'r745'::regclass;
ALTER TABLE r745 ADD PRIMARY KEY (a);
ALTER TABLE r745 DROP CONSTRAINT r745_a_not_null;
CREATE TABLE s745 (a integer CONSTRAINT s745_a NOT NULL NO INHERIT, b integer);
ALTER TABLE s745 ADD PRIMARY KEY (a);
CREATE TABLE pc745 (a integer, b integer);
ALTER TABLE pc745 ADD CONSTRAINT pc745_b NOT NULL b NOT VALID;
CREATE TABLE cc745 () INHERITS (pc745);
SELECT conrelid::regclass::text AS rel, conname, convalidated, conislocal, coninhcount FROM pg_constraint WHERE contype = 'n' AND conrelid IN ('pc745'::regclass, 'cc745'::regclass) ORDER BY rel;
