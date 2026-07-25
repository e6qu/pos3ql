-- ALTER TABLE ... RENAME CONSTRAINT old TO new. Renames a CHECK, a
-- table-level UNIQUE/PRIMARY KEY, or a FOREIGN KEY; the new name must be free
-- (42710) and the old must exist (42704). The renamed constraint keeps its
-- enforcement and is reachable by its new name. Matches PostgreSQL 18.
-- (A single-column UNIQUE/PK is stored as a column flag without a retained
-- name, so multi-column UNIQUE is used here to exercise a key rename.)
DROP TABLE IF EXISTS rc;
CREATE TABLE rc (id int, a int, b int);
INSERT INTO rc VALUES (1, 5, 10);
ALTER TABLE rc ADD CONSTRAINT ck0 CHECK (a > 0);
ALTER TABLE rc ADD CONSTRAINT u UNIQUE (b, id);
ALTER TABLE rc ADD CONSTRAINT ck2 CHECK (a < 100);

-- A CHECK and a UNIQUE rename.
ALTER TABLE rc RENAME CONSTRAINT ck0 TO ck;
ALTER TABLE rc RENAME CONSTRAINT u TO u2;

-- Renaming onto an existing name is 42710; renaming a missing one is 42704.
ALTER TABLE rc RENAME CONSTRAINT ck2 TO ck;
ALTER TABLE rc RENAME CONSTRAINT nope TO whatever;

-- The renamed CHECK is still enforced and droppable by its new name.
INSERT INTO rc VALUES (2, -1, 20);
ALTER TABLE rc DROP CONSTRAINT ck;
ALTER TABLE rc DROP CONSTRAINT u2;
INSERT INTO rc VALUES (3, -1, 10);
SELECT count(*) FROM rc;

DROP TABLE rc;
