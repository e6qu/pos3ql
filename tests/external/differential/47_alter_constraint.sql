-- ALTER TABLE ... ADD/DROP CONSTRAINT. ADD validates every existing row
-- against the new constraint (CHECK 23514, UNIQUE 23505, FK 23503) before
-- attaching it, then it is enforced on later DML; DROP CONSTRAINT removes it
-- by name (a generated name like <table>_<column>_key for a single-column
-- UNIQUE), with IF EXISTS skipping a missing one via a notice. Matches
-- PostgreSQL 18.
DROP TABLE IF EXISTS ch;
DROP TABLE IF EXISTS par;
CREATE TABLE ch (id int, a int, b int);
INSERT INTO ch VALUES (1, 5, 10), (2, 7, 20);

-- ADD CHECK: a satisfied predicate attaches; one violated by an existing row
-- is refused (23514); once attached it is enforced.
ALTER TABLE ch ADD CONSTRAINT ck CHECK (a > 0);
ALTER TABLE ch ADD CONSTRAINT ck2 CHECK (a > 6);
INSERT INTO ch VALUES (3, -1, 30);
ALTER TABLE ch DROP CONSTRAINT ck;
INSERT INTO ch VALUES (4, -1, 40);

-- ADD UNIQUE: enforced after attach; DROP by the generated name lifts it.
ALTER TABLE ch ADD UNIQUE (b);
INSERT INTO ch VALUES (5, 8, 10);
ALTER TABLE ch DROP CONSTRAINT ch_b_key;
INSERT INTO ch VALUES (6, 9, 10);

-- DROP CONSTRAINT of a missing name errors (42704); IF EXISTS skips it.
ALTER TABLE ch DROP CONSTRAINT nope;
ALTER TABLE ch DROP CONSTRAINT IF EXISTS nope;

-- ADD FOREIGN KEY: refused while a child value has no parent (23503); once the
-- orphans are gone it attaches and is enforced; DROP lifts it.
CREATE TABLE par (pid int PRIMARY KEY);
INSERT INTO par VALUES (10), (20);
ALTER TABLE ch ADD CONSTRAINT fk FOREIGN KEY (b) REFERENCES par(pid);
DELETE FROM ch WHERE b NOT IN (10, 20);
ALTER TABLE ch ADD CONSTRAINT fk FOREIGN KEY (b) REFERENCES par(pid);
INSERT INTO ch VALUES (7, 1, 99);
ALTER TABLE ch DROP CONSTRAINT fk;
INSERT INTO ch VALUES (8, 1, 99);

SELECT count(*) FROM ch;
DROP TABLE ch;
DROP TABLE par;
