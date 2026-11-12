-- Referential actions and their errors. Concurrent writers are exercised by
-- the engine's two-session tests (row_lock_conflicts); this fixture pins the
-- single-session values and messages they must keep.
CREATE SCHEMA fk_actions;
SET search_path = fk_actions, public;
CREATE TABLE parents(id integer PRIMARY KEY, code integer UNIQUE, note text);
INSERT INTO parents VALUES (1, 10, 'a'), (2, 20, 'b'), (3, 30, 'c');
CREATE TABLE restricted(parent integer REFERENCES parents ON DELETE RESTRICT ON UPDATE RESTRICT);
CREATE TABLE plain(parent integer REFERENCES parents);
CREATE TABLE cascaded(parent integer REFERENCES parents ON DELETE CASCADE ON UPDATE CASCADE);
CREATE TABLE nulled(parent integer REFERENCES parents ON DELETE SET NULL);
CREATE TABLE by_code(code integer REFERENCES parents(code) ON UPDATE CASCADE);
INSERT INTO restricted VALUES (1);
INSERT INTO plain VALUES (2);
INSERT INTO cascaded VALUES (3), (3);
INSERT INTO nulled VALUES (3);
INSERT INTO by_code VALUES (20);
DELETE FROM parents WHERE id = 1;
UPDATE parents SET id = 11 WHERE id = 1;
DELETE FROM parents WHERE id = 2;
UPDATE parents SET note = 'kept' WHERE id = 2;
UPDATE parents SET code = 21 WHERE id = 2;
SELECT * FROM by_code;
UPDATE parents SET id = 33 WHERE id = 3;
SELECT * FROM cascaded ORDER BY 1;
DELETE FROM parents WHERE id = 33;
SELECT count(*) FROM cascaded;
SELECT * FROM nulled;
INSERT INTO plain VALUES (99);
BEGIN ISOLATION LEVEL REPEATABLE READ;
INSERT INTO plain VALUES (2);
SELECT parent FROM plain ORDER BY 1;
SELECT id FROM parents WHERE id = 2 FOR KEY SHARE;
COMMIT;
RESET search_path;
DROP SCHEMA fk_actions CASCADE;
