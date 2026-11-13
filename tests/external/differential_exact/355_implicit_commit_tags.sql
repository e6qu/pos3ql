-- A simple Query reports its last statement's CommandComplete only after its
-- implicit transaction commits: a deferred constraint that fails there
-- reports no tag, and deferred trigger notices come before the tag.
CREATE TABLE ic355 (a int UNIQUE DEFERRABLE INITIALLY DEFERRED);
INSERT INTO ic355 VALUES (1);
INSERT INTO ic355 VALUES (1);
INSERT INTO ic355 VALUES (2) \; INSERT INTO ic355 VALUES (1);
INSERT INTO ic355 VALUES (1) \; SELECT 7;
INSERT INTO ic355 VALUES (1) \; COMMIT \; SELECT 8;
BEGIN; INSERT INTO ic355 VALUES (1); COMMIT;
-- A foreign key cannot reference a deferrable key.
CREATE TABLE ic355_bad (a int REFERENCES ic355 (a));
CREATE TABLE ic355_parent (a int PRIMARY KEY DEFERRABLE, b int UNIQUE);
CREATE TABLE ic355_bad (a int REFERENCES ic355_parent);
CREATE TABLE ic355_child (a int REFERENCES ic355_parent (b) DEFERRABLE INITIALLY DEFERRED);
INSERT INTO ic355_child VALUES (9);
INSERT INTO ic355_child VALUES (10) \; INSERT INTO ic355_parent VALUES (1, 10);
CREATE TABLE ic355_range (r int4range,
  EXCLUDE USING gist (r WITH &&) DEFERRABLE INITIALLY DEFERRED);
INSERT INTO ic355_range VALUES ('[1,5)') \; INSERT INTO ic355_range VALUES ('[3,7)');
INSERT INTO ic355_range VALUES ('[1,5)'), ('[8,9)') \; INSERT INTO ic355_range VALUES ('[3,7)');
-- BEGIN in a multi-statement Query makes its implicit transaction explicit:
-- ROLLBACK erases the statement before it, and characteristics that a
-- query before it fixed fail.
INSERT INTO ic355 VALUES (1) \; BEGIN \; INSERT INTO ic355 VALUES (3) \; ROLLBACK;
SELECT 1 \; BEGIN ISOLATION LEVEL SERIALIZABLE \; SELECT 2;
SELECT 1 \; BEGIN READ ONLY \; SHOW transaction_read_only \; ROLLBACK;
CREATE TABLE ic355_log (a int);
CREATE FUNCTION ic355_note() RETURNS trigger LANGUAGE plpgsql AS
  $$BEGIN RAISE NOTICE 'deferred %', NEW.a; RETURN NULL; END$$;
CREATE CONSTRAINT TRIGGER ic355_note AFTER INSERT ON ic355_log
  DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION ic355_note();
INSERT INTO ic355_log VALUES (5);
INSERT INTO ic355_log VALUES (6) \; INSERT INTO ic355_log VALUES (7);
SELECT a FROM ic355 ORDER BY a;
SELECT count(*) FROM ic355_log;
DROP TABLE ic355_child, ic355_parent, ic355, ic355_range, ic355_log;
DROP FUNCTION ic355_note();
