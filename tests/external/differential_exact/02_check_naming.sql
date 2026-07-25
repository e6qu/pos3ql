-- CHECK constraint auto-naming fidelity. PostgreSQL names an unnamed CHECK
-- `<table>_<column>_check` when the predicate references exactly one column and
-- `<table>_check` when it references zero or several, disambiguating a
-- collision with the smallest numeric suffix (`_check1`, `_check2`, …). The
-- name is invisible to the SQLSTATE-normalized corpus, so it is pinned here
-- through the violation message (which quotes the constraint name) and through
-- DROP CONSTRAINT by the generated name. Matches PostgreSQL 18.
DROP TABLE IF EXISTS cn1;
DROP TABLE IF EXISTS cn2;
DROP TABLE IF EXISTS cn3;
DROP TABLE IF EXISTS cn4;
DROP TABLE IF EXISTS cn5;

-- Column-level CHECK references exactly its own column: cn1_a_check.
CREATE TABLE cn1 (a int CHECK (a > 0), b int);
INSERT INTO cn1 VALUES (-1, 0);

-- Table-level CHECK referencing one column is named for that column, even
-- though it is written as a table constraint: cn2_a_check. A table-level CHECK
-- referencing two columns falls back to cn2_check.
CREATE TABLE cn2 (a int, b int, CHECK (a > 0), CHECK (a > b));
INSERT INTO cn2 VALUES (-1, -5);
INSERT INTO cn2 VALUES (3, 10);

-- A column-level CHECK that names only *another* column is named for that
-- other column (PostgreSQL keys the name off the referenced Vars, not the
-- owning column): cn3_b_check. One referencing several columns is cn3_check.
CREATE TABLE cn3 (a int CHECK (b > 0), b int, CHECK (a + b > 0));
INSERT INTO cn3 VALUES (5, -1);
INSERT INTO cn3 VALUES (-10, 3);

-- Three predicates over the same single column collide on cn4_a_check, so the
-- second and third disambiguate to cn4_a_check1 and cn4_a_check2. Each insert
-- violates exactly one so the reported name is unambiguous.
CREATE TABLE cn4 (a int CHECK (a > 0), CHECK (a < 100), CHECK (a <> 50));
INSERT INTO cn4 VALUES (-1);
INSERT INTO cn4 VALUES (200);
INSERT INTO cn4 VALUES (50);

-- An explicit name wins and is not disambiguated; a later unnamed CHECK on the
-- same column takes the base generated name.
CREATE TABLE cn5 (a int CONSTRAINT keep_me CHECK (a > 0), CHECK (a < 100));
INSERT INTO cn5 VALUES (-1);
INSERT INTO cn5 VALUES (200);

-- ALTER TABLE ADD CHECK (unnamed) auto-names the same way, and the generated
-- name is what DROP CONSTRAINT must use.
ALTER TABLE cn1 ADD CHECK (b < 10);
INSERT INTO cn1 VALUES (1, 20);
ALTER TABLE cn1 DROP CONSTRAINT cn1_b_check;
INSERT INTO cn1 VALUES (1, 20);
ALTER TABLE cn1 DROP CONSTRAINT cn1_a_check;
INSERT INTO cn1 VALUES (-1, 0);

DROP TABLE cn1;
DROP TABLE cn2;
DROP TABLE cn3;
DROP TABLE cn4;
DROP TABLE cn5;
