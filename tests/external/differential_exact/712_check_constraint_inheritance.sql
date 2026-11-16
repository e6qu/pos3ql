-- A CHECK constraint records whether its table defines it (conislocal) apart
-- from how many parents do (coninhcount): CREATE TABLE and a parent's ALTER
-- TABLE merge an identical CHECK into an inherited one with a notice, a
-- child takes its parent's name, a partition's CHECK becomes its parent's,
-- and one losing its last parent (NO INHERIT, DETACH) stays as the table's
-- own, as does a local one whose parent drops it. An inherited NOT NULL
-- name repeating an inherited CHECK name is kept, so the new table's
-- pg_constraint rows collide (23505).
-- https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/heap.c
CREATE TABLE p1 (a integer, CONSTRAINT x CHECK (a > 0));
CREATE TABLE p2 (a integer CONSTRAINT x NOT NULL, b integer);
CREATE TABLE c1 () INHERITS (p1, p2);
CREATE TABLE c2 () INHERITS (p2, p1);
CREATE TABLE c3 (CONSTRAINT x CHECK (a > 0)) INHERITS (p1);
CREATE TABLE c4 (a integer, CONSTRAINT x CHECK (a > 0)) INHERITS (p1);
CREATE TABLE c5 (CONSTRAINT y CHECK (a > 0)) INHERITS (p2);
CREATE TABLE p3 (b integer CONSTRAINT y NOT NULL);
CREATE TABLE c6 (CONSTRAINT y CHECK (b > 0)) INHERITS (p3);
CREATE TABLE c7 (a integer, CONSTRAINT x CHECK (a > 1)) INHERITS (p1);
SELECT conrelid::regclass::text AS rel, conname, contype, conislocal, coninhcount, connoinherit
FROM pg_constraint WHERE conrelid::regclass::text ~ '^[pc][0-9]$' ORDER BY 1, 2, 3;
ALTER TABLE c4 NO INHERIT p1;
CREATE TABLE c8 () INHERITS (p1);
ALTER TABLE c8 NO INHERIT p1;
ALTER TABLE p1 ADD CONSTRAINT z CHECK (a < 100);
SELECT conrelid::regclass::text AS rel, conname, contype, conislocal, coninhcount, connoinherit
FROM pg_constraint WHERE conrelid::regclass::text ~ '^[pc][0-9]$' ORDER BY 1, 2, 3;
ALTER TABLE p1 DROP CONSTRAINT x;
SELECT conrelid::regclass::text AS rel, conname, contype, conislocal, coninhcount
FROM pg_constraint WHERE conrelid::regclass::text ~ '^[pc][0-9]$' AND contype = 'c' ORDER BY 1, 2, 3;
CREATE TABLE c9 (a integer);
ALTER TABLE c9 ADD CONSTRAINT z CHECK (a < 100);
ALTER TABLE c9 INHERIT p1;
SELECT conrelid::regclass::text AS rel, conname, contype, conislocal, coninhcount
FROM pg_constraint WHERE conrelid = 'c9'::regclass ORDER BY 1, 2, 3;
ALTER TABLE p1 DROP CONSTRAINT z;
SELECT conrelid::regclass::text AS rel, conname, contype, conislocal, coninhcount
FROM pg_constraint WHERE conrelid = 'c9'::regclass ORDER BY 1, 2, 3;
CREATE TABLE pp (a integer, CONSTRAINT k CHECK (a > 0)) PARTITION BY LIST (a);
CREATE TABLE pp1 PARTITION OF pp FOR VALUES IN (1);
CREATE TABLE pp2 (a integer, CONSTRAINT k CHECK (a > 0));
ALTER TABLE pp ATTACH PARTITION pp2 FOR VALUES IN (2);
SELECT conrelid::regclass::text AS rel, conname, contype, conislocal, coninhcount
FROM pg_constraint WHERE conrelid IN ('pp1'::regclass, 'pp2'::regclass) ORDER BY 1, 2, 3;
ALTER TABLE pp DETACH PARTITION pp1;
SELECT conrelid::regclass::text AS rel, conname, contype, conislocal, coninhcount
FROM pg_constraint WHERE conrelid IN ('pp1'::regclass, 'pp2'::regclass) ORDER BY 1, 2, 3;
CREATE TABLE q1 (a integer, b integer);
CREATE TABLE q2 () INHERITS (q1);
CREATE TABLE q3 (a integer, b integer, CONSTRAINT m CHECK (a > 0));
ALTER TABLE q3 INHERIT q1;
ALTER TABLE q1 ADD CHECK (b > 0);
ALTER TABLE q1 ADD CONSTRAINT m CHECK (a > 0);
ALTER TABLE q1 ADD CONSTRAINT m2 CHECK (a > 0);
ALTER TABLE q2 ADD CONSTRAINT m3 CHECK (a > 5);
ALTER TABLE q1 ADD CONSTRAINT m3 CHECK (a > 6);
SELECT conrelid::regclass::text AS rel, conname, conislocal, coninhcount
FROM pg_constraint WHERE conrelid::regclass::text ~ '^q[0-9]$' ORDER BY 1, 2;
ALTER TABLE q3 DROP CONSTRAINT m;
ALTER TABLE q3 DROP CONSTRAINT m2;
ALTER TABLE q1 DROP CONSTRAINT m;
SELECT conrelid::regclass::text AS rel, conname, conislocal, coninhcount
FROM pg_constraint WHERE conrelid::regclass::text ~ '^q[0-9]$' ORDER BY 1, 2;
